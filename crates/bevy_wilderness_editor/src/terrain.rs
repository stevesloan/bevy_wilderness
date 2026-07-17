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
/// A 16-bit grayscale PNG works too: bevy decodes it as `R16Uint` — the same
/// bytes, wrongly tagged for sampling — and `ClipmapPlugin` retags it to
/// `R16Unorm` in `PreUpdate`, before either the renderer or this editor
/// reads it.
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
    /// The paint mask (D4): per-texel 0..1 weight, same dimensions as the
    /// field. Feathered by the brush falloff; erosion (Phase 5) weights its
    /// deltas by it, and sculpt confines itself to it when it's non-empty.
    mask: Vec<f32>,
    /// Count of non-zero mask texels — cheap "is a mask painted at all?".
    mask_nonzero: usize,
    /// Dirty texel rects, kept separate rather than unioned: a toroidal brush
    /// footprint dirties opposite edges, whose union would be nearly the whole
    /// map — quantizing megatexels for a small wrapped stroke.
    dirty: Vec<URect>,
    /// Dirty mask rects, flushed into the overlay visualization texture.
    dirty_mask: Vec<URect>,
    /// Whether the initial full-field sync has flushed. The first flush derives
    /// the display map from freshly decoded (identical) data, so it shouldn't
    /// schedule a re-bake on top of the initial bake.
    synced_once: bool,
}

impl EditableTerrain {
    pub fn new(field: TerrainField) -> Self {
        let mask = vec![0.0; (field.dimensions().x * field.dimensions().y) as usize];
        Self {
            field,
            mask,
            mask_nonzero: 0,
            dirty: Vec::new(),
            dirty_mask: Vec::new(),
            synced_once: false,
        }
    }

    /// Like [`new`](Self::new), but with the whole field marked dirty so the
    /// first sync derives the entire display heightmap from the f32 field.
    pub fn fully_dirty(field: TerrainField) -> Self {
        let mut terrain = Self::new(field);
        terrain.dirty = vec![terrain.field.full_rect()];
        terrain
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

    /// Whether any mask is painted. When `false`, ops treat the whole terrain
    /// as unmasked (everything editable) rather than everything blocked.
    pub fn mask_active(&self) -> bool {
        self.mask_nonzero > 0
    }

    /// The mask weight at a texel: wraps when looping, `0.0` outside a finite
    /// map. `1.0` = fully inside the masked region.
    pub fn mask_weight(&self, x: i64, y: i64) -> f32 {
        match self.field.wrap_texel(x, y) {
            Some((x, y)) => self.mask[(y * self.field.dimensions().x + x) as usize],
            None => 0.0,
        }
    }

    /// Set one mask texel (in-bounds coordinates; clamped to 0..1).
    pub fn set_mask(&mut self, x: u32, y: u32, value: f32) {
        let value = value.clamp(0.0, 1.0);
        let i = (y * self.field.dimensions().x + x) as usize;
        let old = self.mask[i];
        self.mask_nonzero = self.mask_nonzero + (value != 0.0) as usize - (old != 0.0) as usize;
        self.mask[i] = value;
    }

    /// Zero the whole mask (queueing the full overlay for re-display).
    pub fn clear_mask(&mut self) {
        self.mask.fill(0.0);
        self.mask_nonzero = 0;
        self.mark_mask_dirty(self.field.full_rect());
    }

    /// Queue a mask `rect` for flushing to the overlay texture (same contract
    /// as [`mark_dirty`](Self::mark_dirty)).
    pub fn mark_mask_dirty(&mut self, rect: URect) {
        if !rect.is_empty() {
            self.dirty_mask.push(rect);
        }
    }

    fn take_dirty_mask(&mut self) -> Vec<URect> {
        std::mem::take(&mut self.dirty_mask)
    }

    /// Copy a mask rect row-major (undo snapshots; the mask twin of
    /// [`TerrainField::copy_rect`]).
    pub fn mask_copy_rect(&self, rect: URect) -> Vec<f32> {
        let width = self.field.dimensions().x;
        let mut out = Vec::with_capacity((rect.width() * rect.height()) as usize);
        for y in rect.min.y..rect.max.y {
            let row = (y * width + rect.min.x) as usize;
            out.extend_from_slice(&self.mask[row..row + rect.width() as usize]);
        }
        out
    }

    /// Write a mask rect back (undo restore; the mask twin of
    /// [`TerrainField::paste_rect`]). Maintains the non-zero count.
    pub fn mask_paste_rect(&mut self, rect: URect, data: &[f32]) {
        debug_assert_eq!(data.len(), (rect.width() * rect.height()) as usize);
        for (i, y) in (rect.min.y..rect.max.y).enumerate() {
            for (j, x) in (rect.min.x..rect.max.x).enumerate() {
                self.set_mask(x, y, data[i * rect.width() as usize + j]);
            }
        }
    }

    /// Quantize a mask rect into the overlay image (`R8Unorm`).
    fn write_mask_region(&self, image: &mut Image, rect: URect) {
        let Some(data) = image.data.as_deref_mut() else {
            return;
        };
        let width = self.field.dimensions().x;
        for y in rect.min.y..rect.max.y {
            for x in rect.min.x..rect.max.x {
                let i = (y * width + x) as usize;
                data[i] = (self.mask[i] * 255.0).round() as u8;
            }
        }
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
    mut images: ResMut<Assets<Image>>,
    mut clipmaps: Query<(Entity, &mut Clipmap), (With<Editable>, Without<EditableTerrain>)>,
) {
    for (entity, mut clipmap) in &mut clipmaps {
        let Some(image) = images.get(&clipmap.heightmap) else {
            continue; // still loading
        };
        // A PNG heightmap can appear tagged `R16Uint` for a frame before the
        // renderer's `PreUpdate` retag relabels it — pending, not broken.
        // Never treat it as a failure (that would strip `Editable` for good).
        if image.texture_descriptor.format
            == bevy::render::render_resource::TextureFormat::R16Uint
        {
            continue;
        }
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
        // The mask overlay visualization texture: one R8 texel per heightmap
        // texel, zeroed. Assigning it to the clipmap routes it into the
        // terrain material (the renderer's editing API).
        let dims = field.dimensions();
        let overlay = images.add(Image::new(
            bevy::render::render_resource::Extent3d {
                width: dims.x,
                height: dims.y,
                depth_or_array_layers: 1,
            },
            bevy::render::render_resource::TextureDimension::D2,
            vec![0; (dims.x * dims.y) as usize],
            bevy::render::render_resource::TextureFormat::R8Unorm,
            bevy::asset::RenderAssetUsages::MAIN_WORLD
                | bevy::asset::RenderAssetUsages::RENDER_WORLD,
        ));
        clipmap.edit_overlay = Some(overlay);
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

/// Flush dirty mask regions into the overlay visualization texture. Mask edits
/// move no geometry, so no [`TerrainRegionChanged`] and no re-bake — just the
/// overlay tint updating live under the brush.
pub(crate) fn sync_dirty_masks(
    mut terrains: Query<(&mut EditableTerrain, &Clipmap)>,
    mut images: ResMut<Assets<Image>>,
) {
    for (mut terrain, clipmap) in &mut terrains {
        let rects = terrain.take_dirty_mask();
        if rects.is_empty() {
            continue;
        }
        let Some(mut image) = clipmap
            .edit_overlay
            .as_ref()
            .and_then(|overlay| images.get_mut(overlay))
        else {
            continue;
        };
        for rect in rects {
            terrain.write_mask_region(&mut image, rect);
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
