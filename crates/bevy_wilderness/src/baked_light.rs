//! Baked lighting for a clipmap, supplied by the host: an offline baker's sky,
//! bounce and sun visibility over the terrain. While present it replaces the
//! terrain's realtime terms (the environment map's diffuse, the flat ambient
//! and the shadow field's ground shadow); without it, or once the host removes
//! it after the terrain is edited, the terrain lights itself as before. The
//! crate never bakes this itself.

use bevy::{prelude::*, render::render_resource::ShaderType};

/// Most regions one [`TerrainBakedLight`] can hold.
pub const MAX_BAKED_LIGHT_REGIONS: usize = 8;

/// Baked lighting on a [`Clipmap`](crate::Clipmap) entity.
///
/// `atlas` is one `Rgba16Float` image holding every region's two tiles:
/// - **colour:** RGB irradiance / π in the scene's light units (what a
///   Lambertian surface of albedo 1 reflects), A the share of the sun the
///   ground sees, 0..1;
/// - **direction:** RGB the dominant incoming direction as `d * 0.5 + 0.5`,
///   A the rebalance `w = 0.5·n0·d + 0.5` against the normal `n0` it was baked
///   for, so `(0.5·n·d + 0.5) / w` re-aims the light at the shading normal.
#[derive(Component, Clone, Debug)]
pub struct TerrainBakedLight {
    pub atlas: Handle<Image>,
    /// The first covers the whole terrain; later ones are denser patches over
    /// parts of it. Where several cover a point, the last wins, faded in over
    /// [`patch_fade`](Self::patch_fade) at its edge.
    pub regions: Vec<BakedLightRegion>,
    /// Scales the stored colour into the scene's light units.
    pub exposure: f32,
    /// Luminance of the sky the bake was lit by. Environment-map reflections
    /// are scaled by how much of it the ground's baked light holds, so a
    /// shaded gully doesn't mirror a bright sky. 0 = unscaled.
    pub sky_luminance: f32,
    /// Metres over which a patch fades in from its edge.
    pub patch_fade: f32,
}

/// One region of a [`TerrainBakedLight`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BakedLightRegion {
    /// World XZ covered, at the outer edges of its edge texels.
    pub world: Rect,
    /// The colour tile, in atlas UV, at the outer edges of its texels.
    pub color: Rect,
    /// The direction tile, the same size as `color`.
    pub direction: Rect,
}

/// GPU form, matching `BakedLight` in `terrain.wesl`.
#[derive(Clone, Copy, Debug, Default, PartialEq, ShaderType, Reflect)]
pub(crate) struct BakedLightParams {
    /// Regions in use; 0 = no baked light.
    pub(crate) count: u32,
    pub(crate) exposure: f32,
    pub(crate) sky_luminance: f32,
    pub(crate) patch_fade: f32,
    #[reflect(ignore)]
    pub(crate) regions: [BakedRegionGpu; MAX_BAKED_LIGHT_REGIONS],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, ShaderType)]
pub(crate) struct BakedRegionGpu {
    /// min x, min z, max x, max z.
    world: Vec4,
    /// UV min, UV max.
    color: Vec4,
    /// UV min; the tile is `color`'s size.
    direction: Vec4,
}

impl BakedLightParams {
    pub(crate) fn from_baked(baked: &TerrainBakedLight) -> Self {
        if baked.regions.len() > MAX_BAKED_LIGHT_REGIONS {
            warn!(
                "bevy_wilderness: {} baked light regions; only the first {MAX_BAKED_LIGHT_REGIONS} are used",
                baked.regions.len()
            );
        }
        let mut regions = [BakedRegionGpu::default(); MAX_BAKED_LIGHT_REGIONS];
        for (gpu, region) in regions.iter_mut().zip(&baked.regions) {
            let rect = |r: Rect| Vec4::new(r.min.x, r.min.y, r.max.x, r.max.y);
            *gpu = BakedRegionGpu {
                world: rect(region.world),
                color: rect(region.color),
                direction: rect(region.direction),
            };
        }
        Self {
            count: baked.regions.len().min(MAX_BAKED_LIGHT_REGIONS) as u32,
            exposure: baked.exposure,
            sky_luminance: baked.sky_luminance,
            patch_fade: baked.patch_fade.max(1e-3),
            regions,
        }
    }
}
