//! The f32 authoritative height field (design doc P1).
//!
//! Sculpt and erosion mutate this field; the `R16Unorm` display heightmap the
//! renderer samples is *derived* from it by quantizing dirty regions. Never
//! accumulate edits in R16 — one R16 step over a ±1312 m range is ≈ 0.04 m, so
//! erosion's sub-LSB sediment moves would round to zero and stall.

use bevy::{
    asset::RenderAssetUsages,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};

/// The editor's authoritative height field: world-space heights in **meters**,
/// row-major, with the same world↔texel mapping as the renderer's shaders and
/// `Heightfield` (world centered on the origin; keep the math in sync).
///
/// `min`/`max` are the R16 encode range (the [`Clipmap`](bevy_wilderness::Clipmap)
/// fields of the same names), not the current data extrema. When `looping`,
/// texel reads wrap toroidally; otherwise they edge-clamp like the shaders.
pub struct TerrainField {
    heights: Vec<f32>,
    width: u32,
    height: u32,
    texel_size: f32,
    min: f32,
    max: f32,
    looping: bool,
}

impl TerrainField {
    /// Decode an `R16Unorm` heightmap image into an f32 field (meters).
    /// `None` if the format isn't `R16Unorm` or the image data isn't
    /// CPU-resident (`RenderAssetUsages::MAIN_WORLD`).
    pub fn from_image(
        image: &Image,
        texel_size: f32,
        min: f32,
        max: f32,
        looping: bool,
    ) -> Option<Self> {
        if image.texture_descriptor.format != TextureFormat::R16Unorm {
            return None;
        }
        let data = image.data.as_deref()?;
        let (width, height) = (image.width(), image.height());
        let heights = data
            .chunks_exact(2)
            .take((width * height) as usize)
            .map(|px| u16::from_le_bytes([px[0], px[1]]) as f32 / 65535.0 * (max - min) + min)
            .collect();
        Some(Self {
            heights,
            width,
            height,
            texel_size,
            min,
            max,
            looping,
        })
    }

    /// Build a field directly from raw `R16` texels (row-major, `dims.x *
    /// dims.y` of them) — the load path for a 16-bit grayscale PNG, whose
    /// decoder hands back `u16` samples rather than a bevy `Image`. The
    /// `to_r16` inverse.
    pub fn from_r16(texels: &[u16], dims: UVec2, texel_size: f32, min: f32, max: f32, looping: bool) -> Self {
        debug_assert_eq!(texels.len(), (dims.x * dims.y) as usize);
        let heights = texels
            .iter()
            .map(|&t| t as f32 / 65535.0 * (max - min) + min)
            .collect();
        Self {
            heights,
            width: dims.x,
            height: dims.y,
            texel_size,
            min,
            max,
            looping,
        }
    }

    /// A flat field at `initial` meters — the editor's default starting state
    /// (a new terrain), or any host wanting a blank plain to sculpt.
    pub fn flat(
        width: u32,
        height: u32,
        texel_size: f32,
        min: f32,
        max: f32,
        looping: bool,
        initial: f32,
    ) -> Self {
        Self {
            heights: vec![initial; (width * height) as usize],
            width,
            height,
            texel_size,
            min,
            max,
            looping,
        }
    }

    /// Quantize the whole field into a new `R16Unorm` image suitable as a
    /// [`Clipmap`](bevy_wilderness::Clipmap) heightmap. `MAIN_WORLD |
    /// RENDER_WORLD`, so CPU queries (`Heightfield`, `SunVisibility`) keep
    /// working alongside the GPU copy.
    pub fn to_image(&self) -> Image {
        let mut data = Vec::with_capacity(self.heights.len() * 2);
        for &h in &self.heights {
            data.extend_from_slice(&self.quantize(h).to_le_bytes());
        }
        Image::new(
            Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            data,
            TextureFormat::R16Unorm,
            RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
        )
    }

    /// Quantize the whole field to raw `R16` texels (row-major) — the export
    /// path's payload; the same encoding [`Self::to_image`] wraps in an
    /// `Image`.
    pub fn to_r16(&self) -> Vec<u16> {
        self.heights.iter().map(|&h| self.quantize(h)).collect()
    }

    /// Quantize `rect` (texel space, max-exclusive) into `image`, which must be
    /// the same-dimension `R16Unorm` heightmap this field derives. The cheap
    /// dirty-region path: only the touched texels are re-encoded.
    pub fn write_region(&self, image: &mut Image, rect: URect) {
        debug_assert_eq!(image.texture_descriptor.format, TextureFormat::R16Unorm);
        debug_assert_eq!((image.width(), image.height()), (self.width, self.height));
        let Some(data) = image.data.as_deref_mut() else {
            return;
        };
        let x0 = rect.min.x.min(self.width);
        let x1 = rect.max.x.min(self.width);
        let y0 = rect.min.y.min(self.height);
        let y1 = rect.max.y.min(self.height);
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y * self.width + x) as usize;
                let bytes = self.quantize(self.heights[i]).to_le_bytes();
                data[i * 2] = bytes[0];
                data[i * 2 + 1] = bytes[1];
            }
        }
    }

    fn quantize(&self, h: f32) -> u16 {
        (((h - self.min) / (self.max - self.min)).clamp(0.0, 1.0) * 65535.0).round() as u16
    }

    /// Copy `rect`'s heights (texel space, max-exclusive, in-bounds) row-major —
    /// the undo system's tile snapshots.
    pub fn copy_rect(&self, rect: URect) -> Vec<f32> {
        debug_assert!(rect.max.x <= self.width && rect.max.y <= self.height);
        let mut out = Vec::with_capacity((rect.width() * rect.height()) as usize);
        for y in rect.min.y..rect.max.y {
            let row = (y * self.width + rect.min.x) as usize;
            out.extend_from_slice(&self.heights[row..row + rect.width() as usize]);
        }
        out
    }

    /// Write `data` (row-major, `rect`-sized) back into `rect` — the undo
    /// system's restore. The inverse of [`Self::copy_rect`].
    pub fn paste_rect(&mut self, rect: URect, data: &[f32]) {
        debug_assert!(rect.max.x <= self.width && rect.max.y <= self.height);
        debug_assert_eq!(data.len(), (rect.width() * rect.height()) as usize);
        for (i, y) in (rect.min.y..rect.max.y).enumerate() {
            let row = (y * self.width + rect.min.x) as usize;
            let src = i * rect.width() as usize;
            self.heights[row..row + rect.width() as usize]
                .copy_from_slice(&data[src..src + rect.width() as usize]);
        }
    }

    /// Field dimensions in texels.
    pub fn dimensions(&self) -> UVec2 {
        UVec2::new(self.width, self.height)
    }

    /// World size of one texel, in meters.
    pub fn texel_size(&self) -> f32 {
        self.texel_size
    }

    /// The R16 encode range (world heights that quantize to 0 / 65535).
    pub fn min_max(&self) -> (f32, f32) {
        (self.min, self.max)
    }

    /// Whether texel reads wrap toroidally (the terrain repeats).
    pub fn looping(&self) -> bool {
        self.looping
    }

    /// The whole field as a texel rect (max-exclusive), e.g. to mark everything
    /// dirty.
    pub fn full_rect(&self) -> URect {
        URect::new(0, 0, self.width, self.height)
    }

    /// Half the world extent on each axis; the world is centered on the origin.
    pub fn half_extent(&self) -> Vec2 {
        Vec2::new(self.width as f32, self.height as f32) * self.texel_size * 0.5
    }

    /// Whether `xz` lies over the field's world footprint. Always `true` when
    /// `looping` (the terrain repeats everywhere).
    pub fn contains(&self, xz: Vec2) -> bool {
        if self.looping {
            return true;
        }
        let h = self.half_extent();
        xz.x >= -h.x && xz.x <= h.x && xz.y >= -h.y && xz.y <= h.y
    }

    /// One texel's height in meters. Wraps when `looping`, else edge-clamps —
    /// both matching the shaders.
    pub fn get(&self, x: i64, y: i64) -> f32 {
        let (x, y) = if self.looping {
            (
                x.rem_euclid(self.width as i64) as u32,
                y.rem_euclid(self.height as i64) as u32,
            )
        } else {
            (
                x.clamp(0, self.width as i64 - 1) as u32,
                y.clamp(0, self.height as i64 - 1) as u32,
            )
        };
        self.heights[(y * self.width + x) as usize]
    }

    /// Set one texel's height in meters. `x`/`y` must be in bounds; callers
    /// working toroidally wrap their coordinates first (see [`Self::wrap_texel`]).
    pub fn set(&mut self, x: u32, y: u32, h: f32) {
        self.heights[(y * self.width + x) as usize] = h;
    }

    /// Resolve possibly-out-of-range texel coordinates to storage indices:
    /// wraps when `looping` (a brush footprint crossing the seam lands on the
    /// far side, D2), else `None` for out-of-bounds — the write is dropped, not
    /// clamped (clamping would pile a brush's whole overhang onto the edge row).
    pub fn wrap_texel(&self, x: i64, y: i64) -> Option<(u32, u32)> {
        if self.looping {
            Some((
                x.rem_euclid(self.width as i64) as u32,
                y.rem_euclid(self.height as i64) as u32,
            ))
        } else if (0..self.width as i64).contains(&x) && (0..self.height as i64).contains(&y) {
            Some((x as u32, y as u32))
        } else {
            None
        }
    }

    /// Resolve a possibly-out-of-range texel rect (max-exclusive) to in-bounds
    /// pieces: clamped to one piece when finite, or split across the seam into
    /// up to four when `looping` — so a wrapped brush footprint dirties the far
    /// side's texels instead of unioning into a whole-map rect.
    pub fn wrap_rect(&self, min: IVec2, max: IVec2) -> Vec<URect> {
        // Split one axis into in-bounds spans (max-exclusive).
        let axis = |lo: i32, hi: i32, n: i32| -> Vec<(u32, u32)> {
            if !self.looping {
                let (lo, hi) = (lo.clamp(0, n), hi.clamp(0, n));
                if lo < hi {
                    return vec![(lo as u32, hi as u32)];
                }
                return vec![];
            }
            if hi - lo >= n {
                return vec![(0, n as u32)];
            }
            let lo_wrapped = lo.rem_euclid(n);
            let hi_wrapped = lo_wrapped + (hi - lo);
            if hi_wrapped <= n {
                vec![(lo_wrapped as u32, hi_wrapped as u32)]
            } else {
                vec![(lo_wrapped as u32, n as u32), (0, (hi_wrapped - n) as u32)]
            }
        };
        let xs = axis(min.x, max.x, self.width as i32);
        let ys = axis(min.y, max.y, self.height as i32);
        xs.iter()
            .flat_map(|&(x0, x1)| ys.iter().map(move |&(y0, y1)| URect::new(x0, y0, x1, y1)))
            .collect()
    }

    /// Fractional texel coordinates for a world `xz` — the same mapping as the
    /// vertex shader / `Heightfield` (`uv = xz / (dims * texel) + 0.5`).
    pub fn world_to_texel(&self, xz: Vec2) -> Vec2 {
        let dims = Vec2::new(self.width as f32, self.height as f32);
        (xz / (dims * self.texel_size) + 0.5) * dims
    }

    /// World `xz` for fractional texel coordinates (inverse of
    /// [`Self::world_to_texel`]).
    pub fn texel_to_world(&self, texel: Vec2) -> Vec2 {
        let dims = Vec2::new(self.width as f32, self.height as f32);
        (texel - dims * 0.5) * self.texel_size
    }

    /// The world-space rect covered by a texel rect (for
    /// [`TerrainRegionChanged`](crate::TerrainRegionChanged) events).
    pub fn texel_rect_to_world(&self, rect: URect) -> Rect {
        Rect::from_corners(
            self.texel_to_world(rect.min.as_vec2()),
            self.texel_to_world(rect.max.as_vec2()),
        )
    }

    /// World-space terrain height at `xz` — bilinear, matching the shader's
    /// `terrain_height` and `Heightfield::height`.
    pub fn height_at(&self, xz: Vec2) -> f32 {
        let pos = self.world_to_texel(xz);
        let base = pos.floor();
        let f = pos - base;
        let (x0, y0) = (base.x as i64, base.y as i64);
        let h00 = self.get(x0, y0);
        let h10 = self.get(x0 + 1, y0);
        let h01 = self.get(x0, y0 + 1);
        let h11 = self.get(x0 + 1, y0 + 1);
        (h00 * (1.0 - f.x) + h10 * f.x) * (1.0 - f.y) + (h01 * (1.0 - f.x) + h11 * f.x) * f.y
    }

    /// March a ray against the field and return the surface hit point, if any.
    /// The shared pick every tool uses, so a placed prop lands exactly where the
    /// brush would paint. Returns `None` if the ray starts below the surface,
    /// exits above the terrain ceiling, lands outside a non-looping footprint,
    /// or exceeds `max_dist`.
    pub fn raycast(&self, origin: Vec3, direction: Vec3, max_dist: f32) -> Option<Vec3> {
        let dir = direction.normalize_or_zero();
        if dir == Vec3::ZERO {
            return None;
        }
        let mut clearance = origin.y - self.height_at(origin.xz());
        if clearance <= 0.0 {
            return None;
        }
        let mut t = 0.0;
        while t < max_dist {
            // Clearance-scaled stepping: far above the surface takes big strides,
            // near it fine ones. Not a strict bound on steep slopes, so the cap
            // keeps a stride from tunneling through a ridge.
            let step = (clearance * 0.5).clamp(self.texel_size * 0.25, self.texel_size * 16.0);
            let prev_t = t;
            t += step;
            let p = origin + dir * t;
            if p.y > self.max && dir.y >= 0.0 {
                return None; // rising above the highest terrain — can't hit
            }
            clearance = p.y - self.height_at(p.xz());
            if clearance <= 0.0 {
                // Crossed the surface: bisect [prev_t, t] to the crossing.
                let (mut lo, mut hi) = (prev_t, t);
                for _ in 0..16 {
                    let mid = (lo + hi) * 0.5;
                    let pm = origin + dir * mid;
                    if pm.y - self.height_at(pm.xz()) > 0.0 {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let p = origin + dir * ((lo + hi) * 0.5);
                if !self.contains(p.xz()) {
                    return None; // over the clamped apron beyond a finite edge
                }
                return Some(Vec3::new(p.x, self.height_at(p.xz()), p.z));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_wilderness::Heightfield;

    fn ramp_field() -> TerrainField {
        // 16×16, heights ramp 0..150 along +X, world spans [-8, 8] at texel_size 1.
        let mut field = TerrainField::flat(16, 16, 1.0, 0.0, 200.0, false, 0.0);
        for y in 0..16 {
            for x in 0..16 {
                field.set(x, y, x as f32 * 10.0);
            }
        }
        field
    }

    #[test]
    fn quantize_round_trips_through_image() {
        let field = ramp_field();
        let image = field.to_image();
        let back = TerrainField::from_image(&image, 1.0, 0.0, 200.0, false).unwrap();
        for y in 0..16 {
            for x in 0..16 {
                let (a, b) = (field.get(x, y), back.get(x, y));
                assert!(
                    (a - b).abs() < 200.0 / 65535.0,
                    "texel ({x},{y}): {a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn from_r16_inverts_to_r16() {
        // The PNG load path: raw u16 texels → field → raw u16 must round-trip
        // (the same encoding to_image/to_r16 produce, just from a decoder's
        // buffer instead of a bevy Image).
        let field = ramp_field();
        let texels = field.to_r16();
        let back = TerrainField::from_r16(
            &texels,
            field.dimensions(),
            field.texel_size(),
            0.0,
            200.0,
            false,
        );
        assert_eq!(back.to_r16(), texels);
        for y in 0..16 {
            for x in 0..16 {
                let (a, b) = (field.get(x, y), back.get(x, y));
                assert!((a - b).abs() < 200.0 / 65535.0, "texel ({x},{y}): {a} vs {b}");
            }
        }
    }

    #[test]
    fn matches_renderer_heightfield() {
        // The derived image must read identically through the renderer's
        // Heightfield — they share the world↔texel mapping the shaders use.
        let field = ramp_field();
        let image = field.to_image();
        let hf = Heightfield::new(&image, 1.0, 0.0, 200.0).unwrap();
        for xz in [
            Vec2::new(0.0, 0.0),
            Vec2::new(-5.3, 2.7),
            Vec2::new(6.9, -7.1),
        ] {
            let (a, b) = (field.height_at(xz), hf.height(xz));
            assert!(
                (a - b).abs() < 0.01,
                "at {xz}: field {a} vs heightfield {b}"
            );
        }
    }

    #[test]
    fn bilinear_interpolates_between_texels() {
        let field = ramp_field();
        // Halfway between texel columns 8 (80 m) and 9 (90 m).
        let h = field.height_at(field.texel_to_world(Vec2::new(8.5, 4.0)));
        assert!((h - 85.0).abs() < 1e-3, "got {h}");
    }

    #[test]
    fn looping_reads_wrap() {
        let mut field = TerrainField::flat(8, 8, 1.0, 0.0, 100.0, true, 0.0);
        field.set(7, 3, 50.0);
        assert_eq!(field.get(-1, 3), 50.0); // -1 wraps to column 7
        assert_eq!(field.get(15, 3), 50.0); // 15 wraps to column 7
        assert!(field.contains(Vec2::new(1e6, 1e6)));
    }

    #[test]
    fn wrap_rect_splits_across_the_seam() {
        let looping = TerrainField::flat(16, 16, 1.0, 0.0, 100.0, true, 0.0);
        // Footprint hanging off the -X edge: two pieces, near edge + far edge.
        let pieces = looping.wrap_rect(IVec2::new(-2, 4), IVec2::new(3, 8));
        assert_eq!(
            pieces,
            vec![URect::new(14, 4, 16, 8), URect::new(0, 4, 3, 8)]
        );
        // Wider than the map on X: collapses to the full span once.
        let pieces = looping.wrap_rect(IVec2::new(-20, 4), IVec2::new(20, 8));
        assert_eq!(pieces, vec![URect::new(0, 4, 16, 8)]);

        let finite = TerrainField::flat(16, 16, 1.0, 0.0, 100.0, false, 0.0);
        // Same overhang on a finite map: clamped to one in-bounds piece.
        let pieces = finite.wrap_rect(IVec2::new(-2, 4), IVec2::new(3, 8));
        assert_eq!(pieces, vec![URect::new(0, 4, 3, 8)]);
        assert!(
            finite
                .wrap_rect(IVec2::new(-5, 4), IVec2::new(-2, 8))
                .is_empty()
        );
    }

    #[test]
    fn wrap_texel_wraps_or_rejects() {
        let looping = TerrainField::flat(8, 8, 1.0, 0.0, 100.0, true, 0.0);
        assert_eq!(looping.wrap_texel(-1, 9), Some((7, 1)));
        let finite = TerrainField::flat(8, 8, 1.0, 0.0, 100.0, false, 0.0);
        assert_eq!(finite.wrap_texel(3, 4), Some((3, 4)));
        assert_eq!(finite.wrap_texel(-1, 4), None);
        assert_eq!(finite.wrap_texel(3, 8), None);
    }

    #[test]
    fn world_texel_mapping_inverts() {
        let field = ramp_field();
        let xz = Vec2::new(-3.2, 5.9);
        let back = field.texel_to_world(field.world_to_texel(xz));
        assert!((back - xz).length() < 1e-4);
    }

    #[test]
    fn raycast_hits_flat_plane() {
        let field = TerrainField::flat(64, 64, 1.0, 0.0, 100.0, false, 10.0);
        // 45° down from (0, 30, 0): drops 20 m to the surface at x = 20.
        let hit = field
            .raycast(
                Vec3::new(0.0, 30.0, 0.0),
                Vec3::new(1.0, -1.0, 0.0).normalize(),
                1000.0,
            )
            .expect("should hit");
        assert!(
            (hit - Vec3::new(20.0, 10.0, 0.0)).length() < 0.1,
            "hit {hit}"
        );
        // Starting below the surface: no hit.
        assert!(
            field
                .raycast(Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y, 1000.0)
                .is_none()
        );
    }

    #[test]
    fn write_region_updates_only_the_rect() {
        let mut field = TerrainField::flat(8, 8, 1.0, 0.0, 100.0, false, 0.0);
        let mut image = field.to_image();
        field.set(2, 2, 100.0);
        field.set(5, 5, 100.0);
        // Write only the rect around (2,2); (5,5) stays stale in the image.
        field.write_region(&mut image, URect::new(2, 2, 3, 3));
        let read = |img: &Image, x: u32, y: u32| {
            let d = img.data.as_deref().unwrap();
            let i = ((y * 8 + x) * 2) as usize;
            u16::from_le_bytes([d[i], d[i + 1]])
        };
        assert_eq!(read(&image, 2, 2), u16::MAX);
        assert_eq!(read(&image, 5, 5), 0);
    }
}
