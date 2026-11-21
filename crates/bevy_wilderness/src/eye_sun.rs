//! How much of the sun the viewer sees past the terrain: the fog's sun share
//! where there's no fog shadow volume ([`HeightFog::shadow`] `None`).
//!
//! Without the volume the fog shader takes all of the fog as sunlit. Scaling
//! the fog's glow toward the sun by the viewer's own share on the CPU instead
//! ([`HeightFog::seen_from`]) costs the GPU nothing: an eye in a ridge's shade
//! looking toward a sun it can't see sees no glowing haze in front of the
//! ridge. What it can't show is shade the eye isn't in: a shaded valley seen
//! from the sun keeps its glow.
//!
//! [`HeightFog::shadow`]: crate::HeightFog::shadow
//! [`HeightFog::seen_from`]: crate::HeightFog::seen_from

use bevy::prelude::*;

use crate::heightfield::Heightfield;
use crate::{Clipmap, FogShadowViewer, TerrainFog};

/// The sun's angular radius, radians: its shadow's edge is this soft.
const SUN_RADIUS: f32 = 0.00465;
/// Metres toward the sun the terrain is searched for what hides it.
const REACH: f32 = 20_000.0;
/// Each step is this share of the distance so far, at least a texel: near
/// ground is searched finely and far ridges, which subtend less, coarsely.
const STEP_GROWTH: f32 = 0.01;
/// Metres the viewer moves, or radians the sun turns, before looking again.
const RECHECK: f32 = 0.05;
const RECHECK_ANGLE: f32 = 1e-4;

/// The viewer's share of the sun past the terrain, 0..1. 1 without a CPU-side
/// heightmap or a sun.
#[derive(Resource, Clone, Copy, PartialEq, Debug)]
pub struct EyeSunlight(pub f32);

impl Default for EyeSunlight {
    fn default() -> Self {
        Self(1.0)
    }
}

/// Where and toward what the last look was taken.
#[derive(Default)]
pub(crate) struct LastLook(Option<(Vec3, Vec3)>);

#[allow(clippy::too_many_arguments)]
pub(crate) fn update_eye_sunlight(
    fog: Res<TerrainFog>,
    clipmaps: Query<&Clipmap>,
    images: Res<Assets<Image>>,
    suns: Query<&GlobalTransform, With<DirectionalLight>>,
    viewers: Query<&GlobalTransform, With<FogShadowViewer>>,
    cameras: Query<(&GlobalTransform, &Camera), With<Camera3d>>,
    mut last: Local<LastLook>,
    mut eye: ResMut<EyeSunlight>,
) {
    if fog.0.shadow.is_some() {
        // Looked at afresh once the volume is gone, wherever the viewer is.
        last.0 = None;
        return;
    }
    let viewer = viewers.iter().next().or_else(|| {
        cameras
            .iter()
            .filter(|(_, camera)| camera.is_active)
            .max_by_key(|(_, camera)| camera.order)
            .map(|(transform, _)| transform)
    });
    let (Some(viewer), Some(sun)) = (viewer, suns.iter().next()) else {
        return;
    };
    let (at, to_sun) = (viewer.translation(), sun.back().as_vec3());
    if let Some((was_at, was_to_sun)) = last.0
        && at.distance(was_at) < RECHECK
        && to_sun.angle_between(was_to_sun) < RECHECK_ANGLE
    {
        return;
    }
    let field = clipmaps.iter().next().and_then(|clipmap| {
        Heightfield::new(images.get(&clipmap.heightmap)?, clipmap.texel_size, clipmap.min, clipmap.max)
            .map(|field| (field, clipmap.texel_size))
    });
    let Some((field, texel)) = field else {
        eye.set_if_neq(EyeSunlight(1.0));
        return;
    };
    last.0 = Some((at, to_sun));
    eye.set_if_neq(EyeSunlight(sunlight(&field, texel, at, to_sun)));
}

/// The share of the sun's disc seen from `eye` toward `to_sun` (unit) over
/// `field`: how far the sun stands above the highest ground in its direction,
/// across its disc's width. A flat world, as the shadow field is.
fn sunlight(field: &Heightfield, texel: f32, eye: Vec3, to_sun: Vec3) -> f32 {
    let across = Vec2::new(to_sun.x, to_sun.z);
    let flat = across.length();
    if flat < 1e-4 {
        return if to_sun.y > 0.0 { 1.0 } else { 0.0 };
    }
    let direction = across / flat;
    let sun = to_sun.y.atan2(flat);
    let mut horizon = f32::NEG_INFINITY;
    let mut distance = texel.max(0.1);
    while distance < REACH {
        let point = eye.xz() + direction * distance;
        if !field.contains(Vec3::new(point.x, 0.0, point.y)) {
            break;
        }
        horizon = horizon.max((field.height(point) - eye.y).atan2(distance));
        if horizon > sun + SUN_RADIUS {
            return 0.0;
        }
        distance += (distance * STEP_GROWTH).max(texel);
    }
    let edge = ((sun - horizon) / SUN_RADIUS).clamp(-1.0, 1.0);
    // The disc's area above the horizon, taken as a ramp: soft as the eye
    // crosses the shadow's edge, which is all it's for.
    0.5 + 0.5 * edge
}

#[cfg(test)]
mod tests {
    use bevy::asset::RenderAssetUsages;
    use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

    use super::*;

    /// 64 m square at 1 m a texel: flat at 0 but for a 20 m wall along x = 10.
    fn walled() -> Image {
        const N: usize = 64;
        let mut data = Vec::with_capacity(N * N * 2);
        for _y in 0..N {
            for x in 0..N {
                let height = if x == 42 { 20.0 / 100.0 } else { 0.0 };
                data.extend_from_slice(&((height * 65535.0) as u16).to_le_bytes());
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
    fn a_wall_between_the_eye_and_a_low_sun_hides_it() {
        let image = walled();
        let field = Heightfield::new(&image, 1.0, 0.0, 100.0).unwrap();
        let eye = Vec3::new(0.0, 2.0, 0.0);
        let low_east = Vec3::new(1.0, 0.2, 0.0).normalize();
        let low_west = Vec3::new(-1.0, 0.2, 0.0).normalize();
        let high_east = Vec3::new(1.0, 5.0, 0.0).normalize();
        assert_eq!(sunlight(&field, 1.0, eye, low_east), 0.0);
        assert_eq!(sunlight(&field, 1.0, eye, low_west), 1.0);
        assert_eq!(sunlight(&field, 1.0, eye, high_east), 1.0);
    }

    #[test]
    fn the_edge_is_soft() {
        let image = walled();
        let field = Heightfield::new(&image, 1.0, 0.0, 100.0).unwrap();
        let eye = Vec3::new(0.0, 2.0, 0.0);
        // The wall's top, 10 m off and 18 m above the eye, seen through the
        // bilinear filter between texels 41 and 42.
        let grazing = ((field.height(Vec2::new(10.0, 0.0)) - eye.y).atan2(10.0)).tan();
        let to_sun = Vec3::new(1.0, grazing, 0.0).normalize();
        let share = sunlight(&field, 1.0, eye, to_sun);
        assert!(share > 0.0 && share < 1.0, "{share}");
    }
}
