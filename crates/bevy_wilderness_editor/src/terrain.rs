//! Editable-terrain lifecycle: building the f32 field, syncing dirty regions
//! into the display heightmap, and the queries/events host tools bind to.

use bevy::{ecs::system::SystemParam, prelude::*};
use bevy_wilderness::Clipmap;

use crate::field::TerrainField;

/// Marker: make this [`Clipmap`]'s terrain editable. Once the clipmap's
/// heightmap image loads, the editor decodes it into the authoritative f32
/// [`TerrainField`], inserts [`EditableTerrain`], and from then on owns the
/// image's content (edits quantize back into it region-by-region).
///
/// The heightmap must be `R16Unorm` with CPU-resident data
/// (`RenderAssetUsages::MAIN_WORLD | RENDER_WORLD` — the loader default).
///
/// To start from scratch instead of a file, build a [`TerrainField`] yourself
/// (e.g. [`TerrainField::flat`]), `images.add(field.to_image())` for the
/// clipmap's heightmap, and insert [`EditableTerrain::new`] directly — no
/// marker needed.
#[derive(Component, Default)]
pub struct Editable;

/// The editable state of one terrain: the authoritative f32 field (design doc
/// P1) plus the dirty texel region not yet quantized into the display
/// heightmap. Tools mutate `field` and [`mark_dirty`](Self::mark_dirty); the
/// editor flushes the region into the clipmap's image at the end of the frame
/// (`EditorSet::Apply`) and emits [`TerrainRegionChanged`].
#[derive(Component)]
pub struct EditableTerrain {
    pub field: TerrainField,
    /// Dirty texel rects, kept separate rather than unioned: a toroidal brush
    /// footprint dirties opposite edges, whose union would be nearly the whole
    /// map — quantizing megatexels for a small wrapped stroke.
    dirty: Vec<URect>,
    /// Whether the initial full-field sync has flushed. The first flush derives
    /// the display map from freshly decoded (identical) data, so it shouldn't
    /// schedule a re-bake on top of the initial bake.
    synced_once: bool,
}

impl EditableTerrain {
    pub fn new(field: TerrainField) -> Self {
        Self {
            field,
            dirty: Vec::new(),
            synced_once: false,
        }
    }

    /// Like [`new`](Self::new), but with the whole field marked dirty so the
    /// first sync derives the entire display heightmap from the f32 field.
    pub fn fully_dirty(field: TerrainField) -> Self {
        let dirty = vec![field.full_rect()];
        Self {
            field,
            dirty,
            synced_once: false,
        }
    }

    /// Queue `rect` (texel space, max-exclusive, in-bounds) for flushing to the
    /// display heightmap in `EditorSet::Apply`. For a brush footprint that may
    /// overhang the map, pass it through
    /// [`TerrainField::wrap_rect`](crate::TerrainField::wrap_rect) first and
    /// mark each piece.
    pub fn mark_dirty(&mut self, rect: URect) {
        if !rect.is_empty() {
            self.dirty.push(rect);
        }
    }

    fn take_dirty(&mut self) -> Vec<URect> {
        std::mem::take(&mut self.dirty)
    }
}

/// Emitted after edits are flushed into the display heightmap (and after
/// erosion / re-bake in later phases): the terrain under `region` changed.
/// Host tools subscribe to re-snap placed props (design doc §6) — e.g. reset a
/// prop's Y to [`TerrainHeight::sample`] when its XZ falls inside `region`.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct TerrainRegionChanged {
    /// The terrain (clipmap) entity that changed.
    pub terrain: Entity,
    /// World-space XZ rect of the changed region.
    pub region: Rect,
}

/// Once an [`Editable`] clipmap's heightmap image loads (and is a CPU-resident
/// `R16Unorm`), decode it into the authoritative f32 field. Fully dirty, so the
/// first sync re-derives the display map from the field — the terrain renders
/// from f32-derived data from frame one (Phase 1 acceptance).
#[allow(clippy::type_complexity)]
pub(crate) fn init_editable_terrains(
    mut commands: Commands,
    images: Res<Assets<Image>>,
    clipmaps: Query<(Entity, &Clipmap), (With<Editable>, Without<EditableTerrain>)>,
) {
    for (entity, clipmap) in &clipmaps {
        let Some(image) = images.get(&clipmap.heightmap) else {
            continue; // still loading
        };
        let Some(field) = TerrainField::from_image(
            image,
            clipmap.texel_size,
            clipmap.min,
            clipmap.max,
            clipmap.looping,
        ) else {
            warn!(
                "bevy_wilderness_editor: Editable clipmap's heightmap must be R16Unorm with \
                 CPU-resident data (MAIN_WORLD asset usage); terrain will not be editable"
            );
            commands.entity(entity).remove::<Editable>();
            continue;
        };
        commands
            .entity(entity)
            .insert(EditableTerrain::fully_dirty(field));
    }
}

/// Flush each terrain's dirty region: quantize the f32 field's touched texels
/// into the clipmap's `R16Unorm` image (the vertex shader displaces from it, so
/// geometry follows next frame) and emit [`TerrainRegionChanged`]. Runs in
/// `EditorSet::Apply`, after all tools have edited.
pub(crate) fn sync_dirty_regions(
    mut commands: Commands,
    mut terrains: Query<(Entity, &mut EditableTerrain, &Clipmap)>,
    mut images: ResMut<Assets<Image>>,
    mut changed: MessageWriter<TerrainRegionChanged>,
) {
    for (entity, mut terrain, clipmap) in &mut terrains {
        let rects = terrain.take_dirty();
        if rects.is_empty() {
            continue;
        }
        // `get_mut` only when actually dirty — it marks the asset modified,
        // which re-uploads the texture to the GPU.
        let Some(mut image) = images.get_mut(&clipmap.heightmap) else {
            continue;
        };
        for rect in rects {
            terrain.field.write_region(&mut image, rect);
            changed.write(TerrainRegionChanged {
                terrain: entity,
                region: terrain.field.texel_rect_to_world(rect),
            });
        }
        // (Re-)arm the re-bake debounce (D5): inserting replaces the existing
        // timer, so the bake fires ~200 ms after the *last* flush of a stroke.
        // The initial full-field sync is skipped — it derives identical data
        // and the initial bake is already on its way.
        if terrain.synced_once {
            commands
                .entity(entity)
                .insert(crate::rebake::RebakeDebounce::default());
        } else {
            terrain.synced_once = true;
        }
    }
}

/// Terrain height at a world XZ — the query host tools use to snap props to
/// the surface (design doc §6). Reads the authoritative f32 field, so it's
/// current mid-stroke, before the display map has re-quantized.
#[derive(SystemParam)]
pub struct TerrainHeight<'w, 's> {
    terrains: Query<'w, 's, &'static EditableTerrain>,
}

impl TerrainHeight<'_, '_> {
    /// Height in meters at `xz`, from the first editable terrain whose
    /// footprint contains it. `None` if none does.
    pub fn sample(&self, xz: Vec2) -> Option<f32> {
        self.terrains
            .iter()
            .find(|t| t.field.contains(xz))
            .map(|t| t.field.height_at(xz))
    }
}
