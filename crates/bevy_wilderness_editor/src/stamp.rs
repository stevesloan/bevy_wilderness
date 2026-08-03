//! The built-in stamp tool (D11): a grayscale heightfield PNG floats under
//! the cursor as a live **GPU preview** — the renderer's vertex shader
//! composites it, so a half-map stamp moves at full framerate with zero
//! per-frame CPU — and a click **commits** it into the f32 field with the
//! exact same math (bilinear sample, mask weighting, encode-range clamp), so
//! nothing pops.
//!
//! Controls while aiming at terrain: **wheel** = strength (signed — negative
//! carves), **Ctrl+wheel** = scale, **Shift+wheel** = rotation, **Alt+wheel**
//! = edge feather. The wheel is read only while the shared pick hits terrain,
//! so a UI scrolling its own panels (pick blocked via `PointerBlocked`) never
//! fights it.
//!
//! The core owns only the *active* stamp ([`ActiveStamp`]) and its transform
//! ([`StampSettings`]); where stamps come from is the host's business (the
//! default UI ships a folder-gallery, a game can feed manifests). Load one
//! with [`StampData::load_png`] — 8- and 16-bit grayscale both work.

use std::path::Path;

use bevy::{
    asset::RenderAssetUsages,
    input::mouse::AccumulatedMouseScroll,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};
use bevy_wilderness::{Clipmap, ClipmapStamp};

use crate::cursor::TerrainCursor;
use crate::gesture::{TerrainGesture, UndoBuffer};
use crate::terrain::EditableTerrain;
use crate::undo::UndoHistory;

/// A loaded stamp: heights for the CPU commit and a texture for the GPU
/// preview, both derived from the same bake output so preview ≡ commit.
pub struct StampData {
    /// Original texels as loaded, pre-bake, row-major.
    raw: Vec<u16>,
    /// The raw minimum, subtracted during bake.
    floor: u16,
    /// What the current `heights`/`image` were baked with.
    baked_params: BakeParams,
    /// Baked heights 0..1, row-major.
    heights: Vec<f32>,
    dims: UVec2,
    /// The preview texture (`R16Unorm`, render-world only).
    pub image: Handle<Image>,
}

impl StampData {
    /// Load a grayscale PNG stamp from `path` (absolute paths welcome — file
    /// dialogs and galleries hand those out). 8-bit widens losslessly to
    /// 16; color images are luma-converted.
    pub fn load_png(path: &Path, images: &mut Assets<Image>) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read failed: {e}"))?;
        Self::from_png_bytes(&bytes, images)
    }

    /// [`load_png`](Self::load_png) for in-memory PNG bytes.
    pub fn from_png_bytes(bytes: &[u8], images: &mut Assets<Image>) -> Result<Self, String> {
        let luma = image::load_from_memory(bytes)
            .map_err(|e| e.to_string())?
            .into_luma16();
        let dims = UVec2::new(luma.width(), luma.height());
        if dims.x == 0 || dims.y == 0 {
            return Err("stamp has zero dimensions".into());
        }
        let texels = luma.into_raw();
        Ok(Self::from_r16(&texels, dims, images))
    }

    /// Build a stamp from raw `R16` texels (row-major). Bakes with default
    /// params; [`rebake`](Self::rebake) reconciles to the live settings.
    pub fn from_r16(texels: &[u16], dims: UVec2, images: &mut Assets<Image>) -> Self {
        debug_assert_eq!(texels.len(), (dims.x * dims.y) as usize);
        let min = texels.iter().copied().min().unwrap_or(0);
        let max = texels.iter().copied().max().unwrap_or(0);
        // A constant stamp (flat plateau) would bake to nothing if its own
        // value were subtracted as the baseline.
        let floor = if min == max { 0 } else { min };
        let params = BakeParams::default();
        let baked = bake(texels, dims, floor, params);
        let heights = baked.iter().map(|&t| t as f32 / 65535.0).collect();
        let image = make_image(&baked, dims, images);
        Self {
            raw: texels.to_vec(),
            floor,
            baked_params: params,
            heights,
            dims,
            image,
        }
    }

    /// Re-run the bake if `params` differ from what's currently baked; no-op
    /// otherwise. Swaps in a fresh image handle — the texture is render-world
    /// only, so there is no CPU copy to edit in place.
    pub fn rebake(&mut self, params: BakeParams, images: &mut Assets<Image>) {
        if params == self.baked_params {
            return;
        }
        let baked = bake(&self.raw, self.dims, self.floor, params);
        self.heights = baked.iter().map(|&t| t as f32 / 65535.0).collect();
        self.image = make_image(&baked, self.dims, images);
        self.baked_params = params;
    }

    /// Stamp dimensions in texels.
    pub fn dims(&self) -> UVec2 {
        self.dims
    }

    /// Bilinear sample at `uv` — **the CPU twin of the shader's
    /// `textureSampleLevel`** (texel centers at half-integers, edges
    /// clamped). Commit uses this so it lands exactly what the preview
    /// showed; don't "fix" one side without the other.
    pub fn sample(&self, uv: Vec2) -> f32 {
        let pos = uv * self.dims.as_vec2() - 0.5;
        let base = pos.floor();
        let f = pos - base;
        let (x0, y0) = (base.x as i64, base.y as i64);
        let at = |x: i64, y: i64| {
            let x = x.clamp(0, self.dims.x as i64 - 1) as usize;
            let y = y.clamp(0, self.dims.y as i64 - 1) as usize;
            self.heights[y * self.dims.x as usize + x]
        };
        let h00 = at(x0, y0);
        let h10 = at(x0 + 1, y0);
        let h01 = at(x0, y0 + 1);
        let h11 = at(x0 + 1, y0 + 1);
        (h00 * (1.0 - f.x) + h10 * f.x) * (1.0 - f.y) + (h01 * (1.0 - f.x) + h11 * f.x) * f.y
    }
}

/// The user-adjustable half of the bake. Default is the identity bake
/// (baseline removal only).
#[derive(Clone, Copy, PartialEq, Default)]
pub struct BakeParams {
    /// Edge feather as a fraction of the half-extent per axis (0 = hard
    /// edge, 0.5 = falloff reaches the center).
    pub feather: f32,
    /// Signed shift after baseline removal, in stamp units (1 = full
    /// strength). Positive re-adds a floor (a plateau with a cliff edge);
    /// negative keeps only relief above `-offset`, clamping the rest flat.
    pub offset: f32,
}

/// The bake pass: baseline, then offset, then feather — so the feather
/// tapers the adjusted relief (including a deliberate offset pedestal),
/// not the raw export's.
fn bake(raw: &[u16], dims: UVec2, floor: u16, params: BakeParams) -> Vec<u16> {
    if floor == 0 && params == BakeParams::default() {
        return raw.to_vec();
    }
    let inv = dims.as_vec2().recip();
    raw.iter()
        .enumerate()
        .map(|(i, &t)| {
            let mut v = (t.saturating_sub(floor)) as f32 / 65535.0;
            v = (v + params.offset).clamp(0.0, 1.0);
            if params.feather > 0.0 {
                // Texel centers at half-integers, matching `sample` and the
                // shader; distance to the nearest edge in normalized uv.
                let x = i as u32 % dims.x;
                let y = i as u32 / dims.x;
                let uv = (Vec2::new(x as f32, y as f32) + 0.5) * inv;
                let d = uv.min(1.0 - uv).min_element();
                let t = (d / params.feather).clamp(0.0, 1.0);
                v *= t * t * (3.0 - 2.0 * t);
            }
            (v * 65535.0).round() as u16
        })
        .collect()
}

/// The preview texture for a set of baked texels. Render-world only: the CPU
/// side reads `heights`, not the image.
fn make_image(baked: &[u16], dims: UVec2, images: &mut Assets<Image>) -> Handle<Image> {
    let data = baked.iter().flat_map(|t| t.to_le_bytes()).collect();
    images.add(Image::new(
        Extent3d {
            width: dims.x,
            height: dims.y,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::R16Unorm,
        RenderAssetUsages::RENDER_WORLD,
    ))
}

/// The stamp the tool previews and commits. `None` = tool does nothing (a UI
/// should prompt for a stamp). A host sets this from its own source — the
/// default UI's folder gallery, a game's manifest.
#[derive(Resource, Default)]
pub struct ActiveStamp(pub Option<StampData>);

/// The active stamp's transform — what the wheel adjusts and a UI shows as
/// sliders.
#[derive(Resource)]
pub struct StampSettings {
    /// Stamp footprint width in world meters (depth follows the image's
    /// aspect ratio).
    pub size: f32,
    /// Height added where the stamp is full white, meters. Signed: negative
    /// carves.
    pub strength: f32,
    /// Rotation about +Y, radians.
    pub rotation: f32,
    /// Edge feather (see [`BakeParams::feather`]).
    pub feather: f32,
    /// Signed value shift (see [`BakeParams::offset`]).
    pub offset: f32,
}

impl StampSettings {
    /// The bake half of the settings, for [`StampData::rebake`].
    pub fn bake_params(&self) -> BakeParams {
        BakeParams {
            feather: self.feather,
            offset: self.offset,
        }
    }
}

impl Default for StampSettings {
    fn default() -> Self {
        Self {
            size: 1000.0,
            strength: 200.0,
            rotation: 0.0,
            feather: 0.0,
            offset: 0.0,
        }
    }
}

/// Re-bake the active stamp when a bake setting changes. Also catches a
/// freshly armed stamp (loaded at defaults) up to live settings.
pub(crate) fn rebake_on_settings_change(
    mut active: ResMut<ActiveStamp>,
    settings: Res<StampSettings>,
    mut images: ResMut<Assets<Image>>,
) {
    // Peek before the mutable deref so a no-op frame doesn't flag the
    // resource as changed.
    if active
        .bypass_change_detection()
        .0
        .as_ref()
        .is_none_or(|data| data.baked_params == settings.bake_params())
    {
        return;
    }
    if let Some(data) = &mut active.0 {
        data.rebake(settings.bake_params(), &mut images);
    }
}

/// Float the preview under the cursor, adjust settings on the wheel, and
/// commit on click. Runs in `EditorSet::Tools`, gated on the stamp tool.
// Bevy systems legitimately take one param per resource.
#[allow(clippy::too_many_arguments)]
pub(crate) fn drive_stamp_tool(
    cursor: Res<TerrainCursor>,
    active: Res<ActiveStamp>,
    mut settings: ResMut<StampSettings>,
    scroll: Res<AccumulatedMouseScroll>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut history: ResMut<UndoHistory>,
    mut gesture: ResMut<TerrainGesture>,
    mut terrains: Query<(Entity, &mut EditableTerrain, &mut Clipmap)>,
) {
    // Wheel only while aiming at terrain: over a UI panel the pick is blocked
    // (PointerBlocked), so panel scrolling never adjusts the stamp — and the
    // host camera's own wheel binding still works when the tool is inactive.
    let dy = scroll.delta.y;
    if dy != 0.0 && cursor.0.is_some() {
        let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
        let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
        let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
        if ctrl {
            settings.size = (settings.size * 1.1f32.powf(dy)).clamp(1.0, 100_000.0);
        } else if shift {
            settings.rotation =
                (settings.rotation + dy * 5f32.to_radians()).rem_euclid(std::f32::consts::TAU);
        } else if alt {
            settings.feather = (settings.feather + dy * 0.02).clamp(0.0, 0.5);
        } else {
            // Additive with a magnitude-scaled step, so the wheel can cross
            // zero into carving.
            let step = (settings.strength.abs() * 0.1).max(0.5);
            settings.strength = (settings.strength + dy * step).clamp(-5000.0, 5000.0);
        }
    }

    for (entity, mut terrain, mut clipmap) in &mut terrains {
        let desired = match (&active.0, cursor.0) {
            (Some(data), Some(hit)) if hit.terrain == entity => {
                let aspect = data.dims.y as f32 / data.dims.x as f32;
                Some(ClipmapStamp {
                    image: data.image.clone(),
                    center: hit.position.xz(),
                    half_size: Vec2::new(settings.size * 0.5, settings.size * 0.5 * aspect),
                    rotation: settings.rotation,
                    strength: settings.strength,
                    masked: terrain.mask_active(),
                })
            }
            _ => None,
        };
        if clipmap.stamp != desired {
            clipmap.stamp = desired.clone();
        }
        // Commit: one click = one undo entry. The preview keeps floating, so
        // repeated clicks stack stamps.
        if let (Some(stamp), Some(data), Some(hit), true) = (
            &desired,
            &active.0,
            cursor.0,
            buttons.just_pressed(MouseButton::Left),
        ) {
            gesture.begin(&mut history, entity, "Stamp");
            apply_stamp(
                &mut terrain,
                data,
                hit.texel,
                stamp.half_size,
                stamp.rotation,
                stamp.strength,
                &mut gesture,
            );
            gesture.seal(&mut history);
        }
    }
}

/// Drop any floating preview while the stamp tool isn't active — the drive
/// system only runs while it is.
pub(crate) fn clear_stamp_preview(mut clipmaps: Query<&mut Clipmap, With<EditableTerrain>>) {
    for mut clipmap in &mut clipmaps {
        if clipmap.stamp.is_some() {
            clipmap.stamp = None;
        }
    }
}

/// Commit the stamp into the f32 field at fractional texel `center` — the CPU
/// twin of the shader composite: same rotation transform, same bilinear
/// [`StampData::sample`], same mask weighting, same encode-range clamp.
/// Captures undo tiles before mutating (the caller wraps begin/seal).
pub(crate) fn apply_stamp(
    terrain: &mut EditableTerrain,
    data: &StampData,
    center: Vec2,
    half_size: Vec2,
    rotation: f32,
    strength: f32,
    gesture: &mut TerrainGesture,
) {
    let texel_size = terrain.field.texel_size();
    let half_texels = half_size / texel_size;
    let (sin, cos) = rotation.sin_cos();
    // Conservative texel AABB of the rotated footprint.
    let ext = Vec2::new(
        half_texels.x * cos.abs() + half_texels.y * sin.abs(),
        half_texels.x * sin.abs() + half_texels.y * cos.abs(),
    );
    let mut min = (center - ext).floor().as_ivec2();
    let mut max = (center + ext).ceil().as_ivec2() + IVec2::ONE;
    // A footprint wider than a looping map would visit texels twice; cap the
    // window to one period.
    let dims = terrain.field.dimensions().as_ivec2();
    for axis in 0..2 {
        if max[axis] - min[axis] > dims[axis] {
            let mid = (min[axis] + max[axis]) / 2;
            min[axis] = mid - dims[axis] / 2;
            max[axis] = min[axis] + dims[axis];
        }
    }
    let (encode_min, encode_max) = terrain.field.min_max();

    // Snapshot first-touch undo tiles *before* mutating (D8).
    for rect in terrain.field.wrap_rect(min, max) {
        gesture.capture(terrain, UndoBuffer::Height, rect);
    }

    let masked = terrain.mask_active();
    let mut touched = false;
    for y in min.y..max.y {
        for x in min.x..max.x {
            // Texel centers at integer coordinates (shader convention); the
            // unwrapped offset from the stamp center is the shader's
            // shortest toroidal offset because the window spans < one period.
            let rel = (Vec2::new(x as f32, y as f32) - center) * texel_size;
            // World → stamp-local (inverse of the stamp's +Y rotation).
            let local = Vec2::new(cos * rel.x + sin * rel.y, -sin * rel.x + cos * rel.y);
            let uv = local / (half_size * 2.0) + 0.5;
            if !(uv.x > 0.0 && uv.x < 1.0 && uv.y > 0.0 && uv.y < 1.0) {
                continue;
            }
            let Some((tx, ty)) = terrain.field.wrap_texel(x as i64, y as i64) else {
                continue;
            };
            let weight = if masked {
                terrain.mask_weight(x as i64, y as i64)
            } else {
                1.0
            };
            if weight <= 0.0 {
                continue;
            }
            let delta = strength * data.sample(uv) * weight;
            if delta == 0.0 {
                continue;
            }
            let h = terrain.field.get(x as i64, y as i64);
            terrain
                .field
                .set(tx, ty, (h + delta).clamp(encode_min, encode_max));
            touched = true;
        }
    }
    if touched {
        for rect in terrain.field.wrap_rect(min, max) {
            terrain.mark_dirty(rect);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::TerrainField;

    fn stamp_gaussian(images: &mut Assets<Image>) -> StampData {
        // 33×33 radial bump peaking at 1.0 in the middle.
        let n = 33u32;
        let texels: Vec<u16> = (0..n * n)
            .map(|i| {
                let (x, y) = ((i % n) as f32, (i / n) as f32);
                let d = Vec2::new(x, y).distance(Vec2::splat(16.0)) / 16.0;
                ((1.0 - d.min(1.0)) * 65535.0) as u16
            })
            .collect();
        StampData::from_r16(&texels, UVec2::splat(n), images)
    }

    fn terrain(size: u32, looping: bool) -> EditableTerrain {
        EditableTerrain::new(TerrainField::flat(
            size, size, 1.0, 0.0, 200.0, looping, 0.0,
        ))
    }

    #[test]
    fn sample_matches_gpu_conventions() {
        let mut images = Assets::default();
        let data = StampData::from_r16(&[0, 65535, 65535, 0], UVec2::new(2, 2), &mut images);
        // Texel centers at half-integers: uv 0.25 = texel (0,0) exactly.
        assert_eq!(data.sample(Vec2::splat(0.25)), 0.0);
        assert_eq!(data.sample(Vec2::new(0.75, 0.25)), 1.0);
        // Dead center bilinearly mixes all four: 0.5.
        assert!((data.sample(Vec2::splat(0.5)) - 0.5).abs() < 1e-6);
        // Out-of-range clamps to the edge texel's value, never wraps: the
        // left edge texel is 0.0, the right edge texel is 1.0.
        assert_eq!(data.sample(Vec2::new(-0.2, 0.25)), 0.0);
        assert_eq!(data.sample(Vec2::new(1.2, 0.25)), 1.0);
    }

    #[test]
    fn baseline_offset_is_subtracted_at_load() {
        let mut images = Assets::default();
        // An offset export: darkest texel 0.25, not black — without baseline
        // removal the whole footprint would stamp a pedestal.
        let quarter = 16384u16;
        let data = StampData::from_r16(&[quarter, 65535], UVec2::new(2, 1), &mut images);
        assert_eq!(
            data.sample(Vec2::new(0.25, 0.5)),
            0.0,
            "floor drops to zero"
        );
        let peak = data.sample(Vec2::new(0.75, 0.5));
        let relief = (65535 - quarter) as f32 / 65535.0;
        assert!((peak - relief).abs() < 1e-4, "relief preserved, got {peak}");
    }

    #[test]
    fn constant_stamp_survives_baseline_removal() {
        let mut images = Assets::default();
        let data = StampData::from_r16(&[65535; 4], UVec2::new(2, 2), &mut images);
        assert_eq!(data.sample(Vec2::splat(0.5)), 1.0, "plateau not zeroed");
    }

    fn feather(feather: f32) -> BakeParams {
        BakeParams {
            feather,
            ..default()
        }
    }

    fn offset(offset: f32) -> BakeParams {
        BakeParams {
            offset,
            ..default()
        }
    }

    #[test]
    fn feather_rebake_tapers_edges_and_reverts() {
        let mut images = Assets::default();
        let mut data = StampData::from_r16(&[65535; 81], UVec2::splat(9), &mut images);
        let original = data.image.clone();

        data.rebake(feather(0.5), &mut images);
        assert_ne!(data.image, original, "rebake swaps in a fresh texture");
        let center = data.sample(Vec2::splat(0.5));
        let edge = data.sample(Vec2::new(0.5 / 9.0, 0.5));
        assert!(center > 0.9, "center keeps its height, got {center}");
        assert!(edge < 0.1, "edge tapers toward zero, got {edge}");

        let feathered = data.image.clone();
        data.rebake(feather(0.5), &mut images);
        assert_eq!(data.image, feathered, "same value is a no-op");

        data.rebake(feather(0.0), &mut images);
        let restored = data.sample(Vec2::new(0.5 / 9.0, 0.5));
        assert_eq!(restored, 1.0, "raw stamp restored exactly");
    }

    #[test]
    fn positive_offset_readds_a_pedestal() {
        let mut images = Assets::default();
        let mut data = StampData::from_r16(&[0, 65535], UVec2::new(2, 1), &mut images);
        data.rebake(offset(0.25), &mut images);
        let low = data.sample(Vec2::new(0.25, 0.5));
        assert!((low - 0.25).abs() < 1e-4, "floor lifts to 0.25, got {low}");
        let peak = data.sample(Vec2::new(0.75, 0.5));
        assert_eq!(peak, 1.0, "peak clamps at full white");
    }

    #[test]
    fn negative_offset_clips_low_relief_flat() {
        let mut images = Assets::default();
        let mut data = StampData::from_r16(&[0, 32768, 65535], UVec2::new(3, 1), &mut images);
        data.rebake(offset(-0.25), &mut images);
        // Texel centers of a 3×1 at u = 1/6, 3/6, 5/6.
        assert_eq!(
            data.sample(Vec2::new(1.0 / 6.0, 0.5)),
            0.0,
            "low clamps flat"
        );
        let mid = data.sample(Vec2::new(0.5, 0.5));
        assert!((mid - 0.25).abs() < 1e-4, "mid sinks by 0.25, got {mid}");
        let peak = data.sample(Vec2::new(5.0 / 6.0, 0.5));
        assert!((peak - 0.75).abs() < 1e-4, "peak sinks by 0.25, got {peak}");
    }

    #[test]
    fn stamp_raises_the_field_and_is_undoable() {
        let mut images = Assets::default();
        let data = stamp_gaussian(&mut images);
        let mut terrain = terrain(128, false);
        let mut history = UndoHistory::default();
        let mut gesture = TerrainGesture::default();
        gesture.begin(&mut history, Entity::PLACEHOLDER, "Stamp");
        apply_stamp(
            &mut terrain,
            &data,
            Vec2::splat(64.0),
            Vec2::splat(20.0),
            0.0,
            50.0,
            &mut gesture,
        );
        gesture.seal(&mut history);
        let peak = terrain.field.get(64, 64);
        assert!(
            (peak - 50.0).abs() < 2.0,
            "center gets ~full strength, got {peak}"
        );
        assert!(terrain.field.get(64 + 15, 64) < peak, "falls off outward");
        assert_eq!(terrain.field.get(64 + 30, 64), 0.0, "outside untouched");
    }

    #[test]
    fn negative_strength_carves_and_clamps_to_encode_range() {
        let mut images = Assets::default();
        let data = stamp_gaussian(&mut images);
        let mut terrain = terrain(64, false);
        let mut gesture = TerrainGesture::default();
        // Field floor is 0 (encode min): carving must clamp, not underflow.
        apply_stamp(
            &mut terrain,
            &data,
            Vec2::splat(32.0),
            Vec2::splat(10.0),
            0.0,
            -80.0,
            &mut gesture,
        );
        assert_eq!(terrain.field.get(32, 32), 0.0, "clamped at encode min");
    }

    #[test]
    fn looping_stamp_wraps_across_the_seam() {
        let mut images = Assets::default();
        let data = stamp_gaussian(&mut images);
        let mut looping = terrain(64, true);
        let mut gesture = TerrainGesture::default();
        // Centered on the seam corner: all four map corners get material.
        apply_stamp(
            &mut looping,
            &data,
            Vec2::ZERO,
            Vec2::splat(10.0),
            0.0,
            50.0,
            &mut gesture,
        );
        assert!(looping.field.get(0, 0) > 40.0);
        assert!(looping.field.get(63, 63) > 0.0, "wraps to the far corner");

        let mut finite = terrain(64, false);
        apply_stamp(
            &mut finite,
            &data,
            Vec2::ZERO,
            Vec2::splat(10.0),
            0.0,
            50.0,
            &mut gesture,
        );
        assert!(finite.field.get(0, 0) > 40.0);
        assert_eq!(finite.field.get(63, 63), 0.0, "finite drops the overhang");
    }

    #[test]
    fn mask_confines_the_stamp() {
        let mut images = Assets::default();
        let data = stamp_gaussian(&mut images);
        let mut terrain = terrain(64, false);
        // Mask only the left half of the footprint.
        for y in 0..64 {
            for x in 0..32 {
                terrain.set_mask(x, y, 1.0);
            }
        }
        let mut gesture = TerrainGesture::default();
        apply_stamp(
            &mut terrain,
            &data,
            Vec2::splat(32.0),
            Vec2::splat(10.0),
            0.0,
            50.0,
            &mut gesture,
        );
        assert!(terrain.field.get(28, 32) > 0.0, "masked side stamped");
        assert_eq!(terrain.field.get(36, 32), 0.0, "unmasked side untouched");
    }

    #[test]
    fn rotation_swings_the_footprint() {
        let mut images = Assets::default();
        // A 3×1 horizontal bar stamp: wide on X, thin on Y.
        let data = StampData::from_r16(&[65535, 65535, 65535], UVec2::new(3, 1), &mut images);
        let mut gesture = TerrainGesture::default();
        let mut flat = terrain(64, false);
        apply_stamp(
            &mut flat,
            &data,
            Vec2::splat(32.0),
            Vec2::new(12.0, 2.0),
            0.0,
            50.0,
            &mut gesture,
        );
        assert!(flat.field.get(42, 32) > 0.0, "unrotated bar reaches +X");
        assert_eq!(flat.field.get(32, 42), 0.0, "but not +Y");

        let mut rotated = terrain(64, false);
        apply_stamp(
            &mut rotated,
            &data,
            Vec2::splat(32.0),
            Vec2::new(12.0, 2.0),
            std::f32::consts::FRAC_PI_2,
            50.0,
            &mut gesture,
        );
        assert!(rotated.field.get(32, 42) > 0.0, "rotated bar reaches +Y");
        assert_eq!(rotated.field.get(42, 32), 0.0, "and leaves +X");
    }
}
