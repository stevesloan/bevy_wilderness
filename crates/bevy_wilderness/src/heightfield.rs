use bevy::{prelude::*, render::render_resource::TextureFormat};

/// CPU view over a clipmap heightmap. Mirrors `bake.wgsl`'s `terrain_height`, so
/// CPU and GPU agree on where the ground is — keep the two in sync.
///
/// Public under the `editing` feature so editor crates reuse this world↔texel +
/// bilinear math instead of reimplementing it (it must stay in sync with the
/// shaders). Requires an `R16Unorm` heightmap whose data is CPU-resident
/// (`RenderAssetUsages::MAIN_WORLD`, part of the loader default) —
/// [`Heightfield::new`] returns `None` otherwise.
pub struct Heightfield<'a> {
    texels: &'a [u8],
    width: usize,
    height: usize,
    texel_size: f32,
    min: f32,
    max: f32,
}

impl<'a> Heightfield<'a> {
    /// Borrow a CPU view over `image`. `texel_size` is the world size of one texel
    /// in meters; `min`/`max` are the world heights that 0 / 65535 map to (the
    /// [`Clipmap`](crate::Clipmap) fields of the same names).
    pub fn new(image: &'a Image, texel_size: f32, min: f32, max: f32) -> Option<Self> {
        // Single-channel 16-bit heightmap (see `convert/clipmap.py`); other formats
        // aren't decoded — the query reports `None`.
        if image.texture_descriptor.format != TextureFormat::R16Unorm {
            return None;
        }
        Some(Self {
            texels: image.data.as_deref()?,
            width: image.width() as usize,
            height: image.height() as usize,
            texel_size,
            min,
            max,
        })
    }

    /// Heightmap dimensions in texels.
    // Editor-facing accessor; unused internally without the `editing` feature.
    #[cfg_attr(not(feature = "editing"), allow(dead_code))]
    pub fn dimensions(&self) -> UVec2 {
        UVec2::new(self.width as u32, self.height as u32)
    }

    /// World size of one texel, in meters.
    // Editor-facing accessor; unused internally without the `editing` feature.
    #[cfg_attr(not(feature = "editing"), allow(dead_code))]
    pub fn texel_size(&self) -> f32 {
        self.texel_size
    }

    /// Half the world extent on each axis; the world is centered on the origin.
    pub fn half_extent(&self) -> Vec2 {
        Vec2::new(self.width as f32, self.height as f32) * self.texel_size * 0.5
    }

    /// Whether `p` lies over the heightmap's world footprint (XZ test only).
    pub fn contains(&self, p: Vec3) -> bool {
        let h = self.half_extent();
        p.x >= -h.x && p.x <= h.x && p.z >= -h.y && p.z <= h.y
    }

    /// One texel's normalized height (0..1), edge-clamped like the shader's
    /// `clamp(p0, 0, hi)`.
    pub fn texel(&self, x: i64, y: i64) -> f32 {
        let x = x.clamp(0, self.width as i64 - 1) as usize;
        let y = y.clamp(0, self.height as i64 - 1) as usize;
        let i = (y * self.width + x) * 2;
        u16::from_le_bytes([self.texels[i], self.texels[i + 1]]) as f32 / 65535.0
    }

    /// World-space terrain height at `xz` — bilinear, matching `terrain_height`.
    pub fn height(&self, xz: Vec2) -> f32 {
        let uv = xz / (Vec2::new(self.width as f32, self.height as f32) * self.texel_size) + 0.5;
        let pos = uv * Vec2::new(self.width as f32, self.height as f32);
        let base = pos.floor();
        let f = pos - base;
        let (x0, y0) = (base.x as i64, base.y as i64);
        let h00 = self.texel(x0, y0);
        let h10 = self.texel(x0 + 1, y0);
        let h01 = self.texel(x0, y0 + 1);
        let h11 = self.texel(x0 + 1, y0 + 1);
        let h =
            (h00 * (1.0 - f.x) + h10 * f.x) * (1.0 - f.y) + (h01 * (1.0 - f.x) + h11 * f.x) * f.y;
        h * (self.max - self.min) + self.min
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::asset::RenderAssetUsages;
    use bevy::render::render_resource::{Extent3d, TextureDimension};

    /// A 16×16 R16Unorm heightmap; `h(x,y)` gives each texel's raw 16-bit height.
    fn heightmap(h: impl Fn(usize, usize) -> u16) -> Image {
        const N: usize = 16;
        let mut data = Vec::with_capacity(N * N * 2);
        for y in 0..N {
            for x in 0..N {
                data.extend_from_slice(&h(x, y).to_le_bytes());
            }
        }
        Image::new(
            Extent3d {
                width: N as u32,
                height: N as u32,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            data,
            TextureFormat::R16Unorm,
            RenderAssetUsages::MAIN_WORLD,
        )
    }

    #[test]
    fn height_decodes_and_centers_on_origin() {
        // Wall (max height) on the +X columns, flat (0) elsewhere. min..max = 0..100.
        let img = heightmap(|x, _| if x >= 12 { u16::MAX } else { 0 });
        let field = Heightfield::new(&img, 1.0, 0.0, 100.0).unwrap();
        // texel_size 1, width 16 -> world spans [-8, 8]; column 13 is world x = 5.
        assert!((field.height(Vec2::new(5.0, 0.0)) - 100.0).abs() < 1e-2);
        assert!(field.height(Vec2::new(-5.0, 0.0)).abs() < 1e-2);
        assert!(field.contains(Vec3::new(7.0, 0.0, 0.0)));
        assert!(!field.contains(Vec3::new(9.0, 0.0, 0.0)));
    }
}
