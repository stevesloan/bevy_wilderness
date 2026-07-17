//! Runtime terrain replacement: start a **new** terrain (a fresh flat plain —
//! the editor's default starting state) or **load** a heightmap file into an
//! already-running terrain. Both swap the authoritative field and the display
//! heightmap on a live `Clipmap`, keeping the same world footprint and R16
//! encode range.
//!
//! "New" builds a flat field; "Load" reads a file from disk synchronously — a
//! `.ktx2` R16 master (via bevy's KTX2 decoder) or a 16-bit grayscale PNG (via
//! the `image` crate, the same lossless path a physics pipeline reads). Loading
//! goes straight through the filesystem rather than the asset server so an
//! *absolute* path from a host's file dialog works (asset paths are rooted at
//! the assets directory); a failed load leaves the current terrain untouched.
//!
//! After either swap the whole field is marked dirty, so the standard
//! `EditorSet::Apply` path re-quantizes the display map, emits
//! [`TerrainRegionChanged`](crate::TerrainRegionChanged) over the full map
//! (host props re-snap to the new surface), and arms the D5 re-bake (fresh
//! shading for the swapped-in heightmap). The mask overlay is reset so
//! `init_edit_overlays` rebuilds it at the new resolution, and the undo history
//! is cleared (its tile snapshots belong to the replaced field).

use std::path::{Path, PathBuf};

use bevy::{
    image::{CompressedImageFormats, ktx2_buffer_to_image},
    prelude::*,
};
use bevy_wilderness::Clipmap;

use crate::{
    field::TerrainField,
    terrain::{Editable, EditableTerrain},
    undo::UndoHistory,
};

/// Reset a terrain to a fresh flat plain — the editor's default starting state,
/// requestable again at any time (e.g. a UI's "New terrain" button). Keeps the
/// terrain's world footprint and R16 encode range; `size` sets the new
/// resolution in texels (square, so only texel density changes) and `height`
/// the flat starting height in meters.
#[derive(Message, Debug, Clone)]
pub struct NewTerrainRequested {
    pub terrain: Entity,
    /// New heightmap resolution in texels (square).
    pub size: u32,
    /// Flat starting height in meters (e.g. `0.0` for a sea-level plain).
    pub height: f32,
}

/// Load a heightmap file into a live terrain, replacing its field and display
/// map (e.g. a UI's "Load terrain" button after a file dialog). `path` may be
/// absolute. The format is chosen by extension — `.png` is 16-bit grayscale,
/// anything else is R16 KTX2. Completion arrives as [`TerrainLoaded`]; a failed
/// load leaves the current terrain untouched.
#[derive(Message, Debug, Clone)]
pub struct LoadRequested {
    pub terrain: Entity,
    pub path: PathBuf,
}

/// A [`LoadRequested`] finished. `error` is `None` on success; a UI shows this
/// as its status line.
#[derive(Message, Debug, Clone)]
pub struct TerrainLoaded {
    pub terrain: Entity,
    pub path: PathBuf,
    pub error: Option<String>,
}

/// Build a fresh flat field for each [`NewTerrainRequested`] and swap it in.
pub(crate) fn apply_new_terrain(
    mut requests: MessageReader<NewTerrainRequested>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut history: ResMut<UndoHistory>,
    mut clipmaps: Query<(&mut Clipmap, Option<&EditableTerrain>)>,
) {
    for req in requests.read() {
        let Ok((mut clipmap, existing)) = clipmaps.get_mut(req.terrain) else {
            continue;
        };
        let Some(world) = world_size(existing) else {
            warn!("NewTerrainRequested for a terrain that isn't editable yet; ignoring");
            continue;
        };
        let texel_size = world / req.size as f32;
        let field = TerrainField::flat(
            req.size,
            req.size,
            texel_size,
            clipmap.min,
            clipmap.max,
            clipmap.looping,
            req.height,
        );
        let heightmap = images.add(field.to_image());
        swap_in(&mut commands, req.terrain, &mut clipmap, &mut history, field, heightmap);
    }
}

/// Load each [`LoadRequested`]'s file and swap it in, reporting via
/// [`TerrainLoaded`]. Synchronous: an explicit user action tolerates a brief
/// decode hitch, and a failed decode never touches the terrain.
pub(crate) fn apply_load(
    mut requests: MessageReader<LoadRequested>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut history: ResMut<UndoHistory>,
    mut loaded: MessageWriter<TerrainLoaded>,
    mut clipmaps: Query<(&mut Clipmap, Option<&EditableTerrain>)>,
) {
    for req in requests.read() {
        let Ok((mut clipmap, existing)) = clipmaps.get_mut(req.terrain) else {
            continue;
        };
        let Some(world) = world_size(existing) else {
            warn!("LoadRequested for a terrain that isn't editable yet; ignoring");
            continue;
        };
        let error = match load_field(&req.path, world, clipmap.min, clipmap.max, clipmap.looping) {
            Ok(field) => {
                let heightmap = images.add(field.to_image());
                swap_in(&mut commands, req.terrain, &mut clipmap, &mut history, field, heightmap);
                None
            }
            Err(error) => Some(error),
        };
        loaded.write(TerrainLoaded {
            terrain: req.terrain,
            path: req.path.clone(),
            error,
        });
    }
}

/// The current world extent (meters, square) from an editable terrain's field —
/// preserved across a swap so a new/loaded map keeps the same footprint.
fn world_size(existing: Option<&EditableTerrain>) -> Option<f32> {
    existing.map(|t| t.field.dimensions().x as f32 * t.field.texel_size())
}

/// Point `clipmap` at the new field's display heightmap and replace its
/// editable state, marking everything dirty so the Apply path re-quantizes,
/// re-snaps props, and re-bakes. Shared by the new-terrain and load paths.
fn swap_in(
    commands: &mut Commands,
    terrain: Entity,
    clipmap: &mut Clipmap,
    history: &mut UndoHistory,
    field: TerrainField,
    heightmap: Handle<Image>,
) {
    clipmap.texel_size = field.texel_size();
    clipmap.heightmap = heightmap;
    // Force init_edit_overlays to rebuild the overlay at the new resolution.
    clipmap.edit_overlay = None;
    let mut editable = EditableTerrain::new(field);
    // Full-field dirty: the Apply path re-quantizes the whole display map,
    // emits TerrainRegionChanged over the entire footprint (props re-snap to
    // the new surface), and arms the re-bake so shading matches the new map.
    editable.mark_dirty(editable.field.full_rect());
    commands
        .entity(terrain)
        // Drop the load marker in case a swap pre-empts an in-flight decode.
        .remove::<Editable>()
        .insert(editable);
    // The old tile snapshots describe a field that no longer exists.
    history.clear();
}

/// Read `path` and decode it into an f32 field, sizing texels so the map keeps
/// the current `world` extent. `.png` is 16-bit grayscale; anything else is
/// R16 KTX2. `Err` carries a message a UI shows and leaves the terrain intact.
fn load_field(
    path: &Path,
    world: f32,
    min: f32,
    max: f32,
    looping: bool,
) -> Result<TerrainField, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read failed: {e}"))?;
    let is_png = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
    if is_png {
        // The interchange format: standard 16-bit grayscale, lossless for R16.
        let luma = image::load_from_memory(&bytes)
            .map_err(|e| e.to_string())?
            .into_luma16();
        let dims = UVec2::new(luma.width(), luma.height());
        if dims.x == 0 || dims.y == 0 {
            return Err("heightmap has zero dimensions".into());
        }
        Ok(TerrainField::from_r16(
            &luma.into_raw(),
            dims,
            world / dims.x as f32,
            min,
            max,
            looping,
        ))
    } else {
        // The engine master: R16 KTX2, decoded by the same loader a restart uses.
        let image = ktx2_buffer_to_image(&bytes, CompressedImageFormats::NONE, false)
            .map_err(|e| e.to_string())?;
        let dims = UVec2::new(image.width(), image.height());
        if dims.x == 0 || dims.y == 0 {
            return Err("heightmap has zero dimensions".into());
        }
        TerrainField::from_image(&image, world / dims.x as f32, min, max, looping)
            .ok_or_else(|| "KTX2 heightmap must be R16_UNORM with CPU-resident data".into())
    }
}
