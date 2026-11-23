//! How much of the mist around the viewer the terrain leaves in the sun: the
//! mist's colour where there's no fog shadow volume ([`HeightFog::shadow`]
//! `None`).
//!
//! Without the volume the fog shader takes all of the mist as sunlit, so mist
//! lying in a valley's shade shows the sun's white where it should show only
//! the sky's blue. Taking the sun's even share out of the mist's colour on the
//! CPU, by the share of the mist that's shaded ([`HeightFog::seen_from`]),
//! costs the GPU nothing. It's one share for all the mist in view: a sunlit
//! bank beside a shaded one takes the colour between them.
//!
//! The share is an average over the mist for a long way around the viewer and
//! not the viewer's own shade ([`EyeSunlight`](crate::EyeSunlight)), which
//! would shift the mist's colour in every direction at a shadow's edge.
//!
//! [`HeightFog::shadow`]: crate::HeightFog::shadow
//! [`HeightFog::seen_from`]: crate::HeightFog::seen_from

use bevy::prelude::*;

use crate::heightfield::Heightfield;
use crate::{Clipmap, FogShadowViewer, TerrainFog};

/// Columns of mist across the terrain, each way.
const CELLS: usize = 64;
/// Metres over which a column's weight falls to 1/e of one at the viewer.
const WIDTH: f32 = 500.0;
/// The weight every column keeps however far off: with no mist nearby, the
/// share is the whole terrain's.
const EVERYWHERE: f32 = 1e-3;
/// Metres the viewer moves before the share is taken again.
const RECHECK: f32 = 5.0;
/// Radians the sun turns before the columns are shaded again.
const RECHECK_ANGLE: f32 = 1e-3;
/// The share moves in steps this size, too small to see: every change is
/// written to every fogged material.
const STEP: f32 = 1.0 / 64.0;

/// The share of the mist around the viewer that the sun reaches past the
/// terrain, 0..1. 1 without mist, a CPU-side heightmap or a sun.
#[derive(Resource, Clone, Copy, PartialEq, Debug)]
pub struct MistSunlight(pub f32);

impl Default for MistSunlight {
    fn default() -> Self {
        Self(1.0)
    }
}

/// What a [`Shade`] was worked out from.
#[derive(Clone, Copy, PartialEq)]
struct Source {
    heightmap: AssetId<Image>,
    texel_size: f32,
    min: f32,
    max: f32,
    base_height: f32,
    falloff: f32,
}

/// Each column's mist and the part of it in the sun, over the whole terrain.
pub(crate) struct Shade {
    source: Source,
    to_sun: Vec3,
    half_extent: Vec2,
    /// `CELLS` × `CELLS`, rows along z: mist, sunlit mist.
    columns: Vec<[f32; 2]>,
}

impl Shade {
    fn new(field: &Heightfield, source: Source, to_sun: Vec3) -> Self {
        let half_extent = field.half_extent();
        let cell = half_extent * 2.0 / CELLS as f32;
        // Mist above `height`, as a share of a column standing on `base_height`.
        let above = |height: f32| {
            (-source.falloff * (height - source.base_height))
                .clamp(-60.0, 60.0)
                .exp()
        };
        let across = Vec2::new(to_sun.x, to_sun.z);
        let flat = across.length();
        let step = (cell.min_element() * 0.5).max(source.texel_size);
        let mut columns = Vec::with_capacity(CELLS * CELLS);
        for row in 0..CELLS {
            for column in 0..CELLS {
                let at = -half_extent + (Vec2::new(column as f32, row as f32) + 0.5) * cell;
                let ground = field.height(at);
                let mist = above(ground);
                let sunlit = if to_sun.y <= 0.0 {
                    0.0
                } else if flat < 1e-4 {
                    mist
                } else {
                    // The height the sun clears every ridge from, over this
                    // column: the mist under it is shaded.
                    let (toward, rise) = (across / flat, to_sun.y / flat);
                    let mut ceiling = ground;
                    let mut distance = step;
                    // Past where a ray from the ground clears the highest
                    // terrain, nothing reaches down to this column.
                    while ground + distance * rise < source.max {
                        let point = at + toward * distance;
                        if !field.contains(Vec3::new(point.x, 0.0, point.y)) {
                            break;
                        }
                        ceiling = ceiling.max(field.height(point) - distance * rise);
                        distance += step;
                    }
                    above(ceiling)
                };
                columns.push([mist, sunlit]);
            }
        }
        Self {
            source,
            to_sun,
            half_extent,
            columns,
        }
    }

    /// The sunlit share of the mist around `viewer`, columns weighing less
    /// with distance.
    fn share(&self, viewer: Vec2) -> f32 {
        let cell = self.half_extent * 2.0 / CELLS as f32;
        let (mut mist, mut sunlit) = (0.0f64, 0.0f64);
        for (index, [column_mist, column_sunlit]) in self.columns.iter().enumerate() {
            let at = -self.half_extent
                + (Vec2::new((index % CELLS) as f32, (index / CELLS) as f32) + 0.5) * cell;
            let weight = f64::from((-at.distance(viewer) / WIDTH).exp() + EVERYWHERE);
            mist += weight * f64::from(*column_mist);
            sunlit += weight * f64::from(*column_sunlit);
        }
        if mist <= 0.0 {
            return 1.0;
        }
        (sunlit / mist) as f32
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn update_mist_sunlight(
    fog: Res<TerrainFog>,
    clipmaps: Query<&Clipmap>,
    images: Res<Assets<Image>>,
    mut image_events: MessageReader<AssetEvent<Image>>,
    suns: Query<&GlobalTransform, With<DirectionalLight>>,
    viewers: Query<&GlobalTransform, With<FogShadowViewer>>,
    cameras: Query<(&GlobalTransform, &Camera), With<Camera3d>>,
    mut shade: Local<Option<Shade>>,
    mut last: Local<Option<Vec3>>,
    mut mist: ResMut<MistSunlight>,
) {
    let edited = image_events.read().any(|event| {
        shade
            .as_ref()
            .is_some_and(|shade| event.is_modified(shade.source.heightmap))
    });
    if edited {
        *shade = None;
    }
    // A mist with no falloff has no top for a shadow to reach.
    if fog.0.shadow.is_some() || fog.0.density <= 0.0 || fog.0.falloff <= 0.0 {
        *last = None;
        mist.set_if_neq(MistSunlight(1.0));
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
    let field = clipmaps.iter().next().and_then(|clipmap| {
        let field = Heightfield::new(
            images.get(&clipmap.heightmap)?,
            clipmap.texel_size,
            clipmap.min,
            clipmap.max,
        )?;
        let source = Source {
            heightmap: clipmap.heightmap.id(),
            texel_size: clipmap.texel_size,
            min: clipmap.min,
            max: clipmap.max,
            base_height: fog.0.base_height,
            falloff: fog.0.falloff,
        };
        Some((field, source))
    });
    let Some((field, source)) = field else {
        *shade = None;
        *last = None;
        mist.set_if_neq(MistSunlight(1.0));
        return;
    };
    let current = shade.as_ref().is_some_and(|shade| {
        shade.source == source && shade.to_sun.angle_between(to_sun) < RECHECK_ANGLE
    });
    if !current {
        *shade = Some(Shade::new(&field, source, to_sun));
        *last = None;
    }
    if last.is_some_and(|was_at| at.distance(was_at) < RECHECK) {
        return;
    }
    *last = Some(at);
    if let Some(shade) = shade.as_ref() {
        let share = (shade.share(at.xz()) / STEP).round() * STEP;
        mist.set_if_neq(MistSunlight(share.clamp(0.0, 1.0)));
    }
}

#[cfg(test)]
mod tests {
    use bevy::asset::RenderAssetUsages;
    use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

    use super::*;

    /// 64 m square at 1 m a texel: flat at 0, with a 20 m wall along x = 10
    /// if `wall`, wide enough that no march steps over its top.
    fn ground(wall: bool) -> Image {
        const N: usize = 64;
        let mut data = Vec::with_capacity(N * N * 2);
        for _y in 0..N {
            for x in 0..N {
                let height = if wall && (41..=43).contains(&x) { 20.0 / 100.0 } else { 0.0 };
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

    /// Mist on the ground, a tenth as thick 10 m up.
    fn shade(image: &Image, to_sun: Vec3) -> Shade {
        let field = Heightfield::new(image, 1.0, 0.0, 100.0).unwrap();
        let source = Source {
            heightmap: AssetId::default(),
            texel_size: 1.0,
            min: 0.0,
            max: 100.0,
            base_height: 0.0,
            falloff: std::f32::consts::LN_10 / 10.0,
        };
        Shade::new(&field, source, to_sun)
    }

    #[test]
    fn mist_on_open_ground_is_all_in_the_sun() {
        let low_east = Vec3::new(1.0, 0.2, 0.0).normalize();
        assert!((shade(&ground(false), low_east).share(Vec2::ZERO) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn no_mist_is_in_a_sun_under_the_horizon() {
        let set = Vec3::new(1.0, -0.1, 0.0).normalize();
        assert_eq!(shade(&ground(false), set).share(Vec2::ZERO), 0.0);
    }

    #[test]
    fn a_wall_shades_the_mist_behind_it() {
        let image = ground(true);
        // The wall stands east of most of the ground: a low sun in the east
        // puts all that in its shadow, one in the west only the strip beyond.
        let from_east = shade(&image, Vec3::new(1.0, 0.2, 0.0).normalize()).share(Vec2::ZERO);
        let from_west = shade(&image, Vec3::new(-1.0, 0.2, 0.0).normalize()).share(Vec2::ZERO);
        let overhead = shade(&image, Vec3::new(0.01, 1.0, 0.0).normalize()).share(Vec2::ZERO);
        assert!(from_east < 0.5, "{from_east}");
        assert!(from_west > from_east && from_west < 1.0, "{from_west}");
        assert!(overhead > 0.95, "{overhead}");
    }

    #[test]
    fn mist_near_the_viewer_weighs_most() {
        let shade = shade(&ground(true), Vec3::new(1.0, 0.2, 0.0).normalize());
        let in_shadow = shade.share(Vec2::new(-20.0, 0.0));
        let in_sun = shade.share(Vec2::new(25.0, 0.0));
        assert!(in_shadow < in_sun, "{in_shadow} {in_sun}");
    }
}
