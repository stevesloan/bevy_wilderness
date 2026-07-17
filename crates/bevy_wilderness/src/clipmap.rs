use std::f32::consts::{FRAC_PI_2, PI};

use bevy::{
    camera::{primitives::Aabb, visibility::NoAutoAabb},
    ecs::system::SystemParam,
    light::NotShadowCaster,
    pbr::ExtendedMaterial,
    prelude::*,
    render::render_resource::TextureFormat,
};

use crate::heightfield::Heightfield;
use crate::material::{DetailParams, DevParams, GridMaterial};
use crate::mesh::{ClipmapPart, ClipmapParts, build_clipmap_parts};
use crate::quality::{TerrainFog, TerrainQuality, inline_fog_params};
use crate::rvt::ClipmapRvt;
use crate::texture::looping_rvt_sampler;

/// Maximum number of terrain material layers, blended per pixel by their
/// procedural slope/height weights (§3.2). Bounded by the `Vec4` lanes in
/// `TerrainParams`; widen those to raise it.
pub const MAX_TERRAIN_LAYERS: usize = 4;

/// Slope-angle band a layer occupies (degrees from horizontal), e.g. grass on
/// flat ground, dirt on mid slopes, rock on cliffs. The layer's weight ramps in
/// over `blend_deg` above `min_deg` and out over `blend_deg` below `max_deg`.
/// Use `min_deg = 0` for "no lower bound" and `max_deg = 90` for "up to vertical".
#[derive(Clone, Debug)]
pub struct SlopeRule {
    pub min_deg: f32,
    pub max_deg: f32,
    /// Angular range (degrees) over which the layer blends in/out at each edge.
    pub blend_deg: f32,
}

/// World-height band a layer occupies (meters), e.g. snow above a snowline. The
/// weight ramps in over `blend` above `min` and out over `blend` below `max`.
/// Use a very negative `min` / very large `max` for an open-ended band.
#[derive(Clone, Debug)]
pub struct HeightRule {
    pub min: f32,
    pub max: f32,
    pub blend: f32,
}

/// A single tiling material layer in a [`Clipmap`]'s splat set. Placement is
/// procedural: a layer appears where its optional [`SlopeRule`] and [`HeightRule`]
/// bands overlap (a layer with neither is present everywhere). No control map.
#[derive(Clone, Debug)]
pub struct TerrainLayer {
    /// World-space size of one texture tile, in meters.
    pub tiling_scale: f32,
    /// Strength of this layer's height relief in height-based blending.
    /// 0 falls back to weight blending; higher lets the layer's alpha-channel
    /// height dominate transitions.
    pub height_blend: f32,
    /// Detail-normal perturbation strength. 0 disables the normal map, 1 is full.
    pub normal_strength: f32,
    /// Multiplier on the layer's ORM roughness channel.
    pub roughness: f32,
    /// Slope-angle band this layer occupies (none = any slope).
    pub slope: Option<SlopeRule>,
    /// World-height band this layer occupies (none = any height).
    pub height: Option<HeightRule>,
}

/// The near-range detail overlay: per-material high-frequency textures blended
/// over the RVT near the camera and faded out with distance, for close-up
/// fidelity the RVT's texel density can't hold.
#[derive(Clone, Debug)]
pub struct DetailConfig {
    /// Per-material detail albedo array (`2d_array`, one slice per layer).
    pub albedo_array: Handle<Image>,
    /// Per-material detail normal array (`2d_array`), for close-up relief.
    pub normal_array: Handle<Image>,
    /// Per-material detail ORM array (`2d_array`), for close-up roughness/AO.
    pub orm_array: Handle<Image>,
    /// World size of one detail tile, in meters.
    pub tiling: f32,
    /// Detail-normal perturbation strength.
    pub normal_strength: f32,
    /// Detail-albedo grain strength.
    pub albedo_strength: f32,
    /// Camera distances (meters) over which the overlay fades out.
    pub near: f32,
    pub far: f32,
}

/// The component defining a clipmap.
/// <https://hhoppe.com/gpugcm.pdf>
#[derive(Component)]
pub struct Clipmap {
    /// Half width of the grid
    /// Stored as half because the full width must be even.
    pub half_width: u32,

    /// Number of LOD levels to generate.
    /// Each next level covers 2x area of previous one.
    pub levels: u32,

    /// Base scale of the LOD square in world units.
    pub base_scale: f32,

    /// Physical size of one texel in meters.
    pub texel_size: f32,

    /// The entity to follow.
    pub target: Entity,

    /// Heightmap texture: single-channel `R16Unorm`, `is_srgb = false`.
    ///
    /// A **CPU-resident heightmap is a supported mode**: keep the image
    /// `RenderAssetUsages::MAIN_WORLD | RENDER_WORLD` (the loader default) and
    /// [`SunVisibility`] / `Heightfield` queries work; an editor may own the
    /// image, mutate its texels (the vertex shader displaces from it, so geometry
    /// follows next frame with no mesh rebuild), and request a `RebakeRequested`
    /// re-bake for the shading (both `editing`-feature API).
    pub heightmap: Handle<Image>,

    /// Albedo texture array (`2d_array`), one slice per layer. The alpha channel
    /// stores per-texel height, used for height-based blending.
    pub albedo_array: Handle<Image>,

    /// Tangent-space normal-map array (`2d_array`), one slice per layer.
    pub normal_array: Handle<Image>,

    /// ORM array (`2d_array`): R = occlusion, G = roughness, B = metallic.
    pub orm_array: Handle<Image>,

    /// Material layers, placed procedurally by slope/height (up to
    /// [`MAX_TERRAIN_LAYERS`]) — see [`TerrainLayer`].
    pub layers: Vec<TerrainLayer>,

    /// Near-range detail overlay (arrays + tiling/strength/fade).
    pub detail: DetailConfig,

    /// Height bounds.
    pub min: f32,
    pub max: f32,

    /// Enable wireframe.
    pub wireframe: bool,

    /// Tile the heightmap toroidally so the terrain repeats
    pub looping: bool,

    /// Editor visualization overlay (`editing` feature): a single-channel 0..1
    /// texture covering the terrain like the heightmap does, tinted into the
    /// surface color — the terrain editor writes its paint mask here. `None`
    /// renders nothing. Assign (or swap) it any time; the materials follow.
    #[cfg(feature = "editing")]
    pub edit_overlay: Option<Handle<Image>>,
}

#[derive(Component)]
pub(crate) struct ClipmapGrid {
    level: u32,
    trim: Entity,
}

impl ClipmapGrid {
    fn scale(&self, base_scale: f32) -> f32 {
        base_scale * 2u32.pow(self.level) as f32
    }
}

/// The clipmap's terrain material, solid + wireframe. Identical across all LOD
/// levels (nothing per-level survives in `GridMaterial`), so it's built once per
/// clipmap and shared by every grid, not rebuilt per level.
#[derive(Component)]
pub(crate) struct ClipmapMaterials {
    solid: Handle<ExtendedMaterial<StandardMaterial, GridMaterial>>,
    wireframe: Handle<ExtendedMaterial<StandardMaterial, GridMaterial>>,
}

/// Marker inserted on a [`Clipmap`] entity once its RVT bake has finished — the
/// terrain's material + self-shadowing are baked and the main pass will render
/// it fully. Callers can gate a loading screen on this so the (one-time) bake
/// pipeline compilation + bake render land before gameplay starts rather than
/// stalling the first live frame.
#[derive(Component)]
pub struct ClipmapReady;

/// Request a re-run of a [`Clipmap`]'s RVT bake (`editing` feature): insert this
/// on the clipmap entity after mutating its heightmap (or moving the sun) and the
/// baked material splat / self-shadow / AO re-bake to match. The bake re-runs the
/// same sentinel-gated pipeline as the initial one; [`ClipmapReady`] is removed
/// while it's in flight and re-inserted when it completes (observe
/// `Added<ClipmapReady>` for the finish signal). The previous bake stays on
/// screen until the new one lands — no unbaked flash.
///
/// A request made while a bake is already in flight is deferred, not dropped: it
/// processes when the in-flight bake finishes.
#[cfg(feature = "editing")]
#[derive(Component)]
pub struct RebakeRequested;

/// Flip an `R16Uint` tag to `R16Unorm` in place. The two formats share a
/// byte-identical texel layout — one 16-bit value per texel — differing only
/// in how samplers interpret it, so this is a relabel, not a transcode.
/// Returns whether the image was retagged.
fn retag_r16uint(image: &mut Image) -> bool {
    if image.texture_descriptor.format == TextureFormat::R16Uint {
        image.texture_descriptor.format = TextureFormat::R16Unorm;
        true
    } else {
        false
    }
}

/// bevy decodes a 16-bit grayscale PNG heightmap as `R16Uint`, which would
/// break every terrain binding (they want Float-filterable samplers) and the
/// CPU readers (`Heightfield` requires `R16Unorm`). Retag it in `PreUpdate`,
/// before `init_clipmaps` or the render world's extraction consume the image
/// — so a PNG heightmap is a first-class asset and a host can ship one file
/// for both terrain and physics (e.g. an Avian collision heightfield reads
/// the same PNG through a standard image decoder).
pub(crate) fn retag_png_heightmaps(
    mut images: ResMut<Assets<Image>>,
    clipmaps: Query<&Clipmap>,
) {
    for clipmap in &clipmaps {
        // `get_mut` only on a mismatch — it marks the asset modified, which
        // would re-upload every heightmap every frame otherwise.
        let needs_retag = images
            .get(&clipmap.heightmap)
            .is_some_and(|image| image.texture_descriptor.format == TextureFormat::R16Uint);
        if needs_retag && let Some(mut image) = images.get_mut(&clipmap.heightmap) {
            retag_r16uint(&mut image);
        }
    }
}

pub(crate) fn init_clipmaps(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
    fog: Res<TerrainFog>,
    quality: Res<TerrainQuality>,
    clipmaps: Query<(Entity, &Clipmap), Added<Clipmap>>,
) {
    // Born with the current inline fog so terrain spawned after startup (when
    // `apply_terrain_quality` won't re-fire) still matches the active tier.
    let initial_fog = inline_fog_params(&fog, quality.fog);
    let size = quality.rvt_size;
    for (entity, clipmap) in clipmaps {
        let parts = build_clipmap_parts(&mut meshes, clipmap.half_width);

        // A looping clipmap tiles the RVT toroidally, so its targets sample with a
        // Repeat address mode; finite terrain keeps the default clamp. Built once
        // and applied to every RVT target below.
        let mut make_rvt_target = |w: u32, h: u32, format: TextureFormat| {
            let mut image = Image::new_target_texture(w, h, format, None);
            if clipmap.looping {
                image.sampler = looping_rvt_sampler();
            }
            images.add(image)
        };

        let rvt_albedo = make_rvt_target(size, size, TextureFormat::Rgba8UnormSrgb);
        let rvt_normal = make_rvt_target(size, size, TextureFormat::Rgba8Unorm);
        // Macro AO (R) + bent normal world X/Z (GB) + cavity (A). Linear. When the
        // ambient gather is disabled it's a 4×4 stub — the binding stays valid but
        // the full-size target (and its bake + sample) are skipped.
        let ao_size = if quality.ambient_gather { size } else { 4 };
        let rvt_ao = make_rvt_target(ao_size, ao_size, TextureFormat::Rgba8Unorm);

        // Editor overlay: the clipmap's texture, or a 1×1 zero stub so the
        // binding is always valid (an editor typically assigns the real one
        // after the heightmap loads; `sync_edit_overlay` picks that up).
        #[cfg(feature = "editing")]
        let edit_overlay = clipmap.edit_overlay.clone().unwrap_or_else(|| {
            images.add(Image::new(
                bevy::render::render_resource::Extent3d::default(),
                bevy::render::render_resource::TextureDimension::D2,
                vec![0],
                TextureFormat::R8Unorm,
                bevy::asset::RenderAssetUsages::RENDER_WORLD,
            ))
        });

        // Quality bits packed into `flags` alongside the per-material wireframe bit
        // (bit1 = ambient gather, bit2 = single-layer detail, bit3 = looping).
        let quality_bits = ((quality.ambient_gather as u32) << 1)
            | ((quality.detail_layers <= 1) as u32) << 2
            | (clipmap.looping as u32) << 3;
        // One material per clipmap, shared by every LOD grid (identical across
        // levels). `wireframe` is the only per-material variant.
        let mut make_material = |wireframe: u32| {
            materials.add(ExtendedMaterial {
                base: StandardMaterial::default(),
                extension: GridMaterial {
                    heightmap: clipmap.heightmap.clone(),
                    rvt_albedo: rvt_albedo.clone(),
                    rvt_normal: rvt_normal.clone(),
                    rvt_ao: rvt_ao.clone(),
                    dev: DevParams::default(),
                    fog: initial_fog.clone(),
                    detail_albedo_array: clipmap.detail.albedo_array.clone(),
                    detail_normal_array: clipmap.detail.normal_array.clone(),
                    detail: DetailParams::from_config(&clipmap.detail),
                    detail_orm_array: clipmap.detail.orm_array.clone(),
                    #[cfg(feature = "editing")]
                    edit_overlay: edit_overlay.clone(),
                    texel_size: clipmap.texel_size,
                    minmax: Vec2::new(clipmap.min, clipmap.max),
                    flags: wireframe | quality_bits,
                },
            })
        };
        let clipmap_materials = ClipmapMaterials {
            solid: make_material(0),
            wireframe: make_material(1),
        };

        commands.entity(entity).insert((
            Transform::default(),
            Visibility::default(),
            clipmap_materials,
            ClipmapRvt {
                albedo: rvt_albedo,
                normal: rvt_normal,
                ao: rvt_ao,
                initialized: false,
                pending_bakes: 0,
                sun_direction: Vec3::ZERO,
                quality: *quality,
                stall_secs: 0.0,
                stall_warned: false,
            },
            parts,
        ));

        for level in 0..clipmap.levels {
            commands.entity(entity).with_child(ClipmapGrid {
                level,
                trim: Entity::PLACEHOLDER,
            });
        }
    }
}

pub(crate) fn init_grids(
    mut commands: Commands,
    clipmaps: Query<(&Clipmap, &ClipmapParts, &ClipmapMaterials)>,
    mut grids: Query<(Entity, &mut ClipmapGrid, &ChildOf), Added<ClipmapGrid>>,
) {
    for (entity, mut grid, child_of) in &mut grids {
        let (clipmap, parts, mats) = clipmaps.get(child_of.parent()).unwrap();

        let filler_width = 2 - clipmap.half_width as i32 % 2;
        let square_width = (clipmap.half_width as i32 - filler_width) / 2;

        commands.entity(entity).insert((
            Transform::from_scale(Vec3::splat(grid.scale(clipmap.base_scale))),
            Visibility::default(),
        ));

        // Height-corrected AABB for frustum culling (the mesh is flat; the vertex
        // shader displaces it). Constant per level, so set once — not per frame.
        let aabb_scale = 2u32.pow(1 + grid.level) as f32;
        let cy = (clipmap.max + clipmap.min) / aabb_scale;
        let hy = (clipmap.max - clipmap.min) / aabb_scale;
        let fix_aabb = |base: &Aabb| {
            let mut a = *base;
            a.center.y = cy;
            a.half_extents.y = hy;
            a
        };

        // Spawn one clipmap part as a child grid mesh (+ a wireframe overlay when
        // enabled), returning its entity. Shares the clipmap's materials.
        let spawn_part = |commands: &mut Commands, part: &ClipmapPart, transform: Transform| {
            let aabb = fix_aabb(&part.aabb);
            let mut e = commands.spawn((
                Mesh3d(part.handle.clone()),
                MeshMaterial3d(mats.solid.clone()),
                NotShadowCaster,
                transform,
                NoAutoAabb,
                aabb,
                ChildOf(entity),
            ));
            if clipmap.wireframe {
                e.with_child((
                    Mesh3d(part.handle.clone()),
                    MeshMaterial3d(mats.wireframe.clone()),
                    NoAutoAabb,
                    aabb,
                ));
            }
            e.id()
        };

        for xy in 0..4 * 4 {
            let x = xy % 4;
            let y = xy / 4;
            if grid.level != 0 && (x == 1 || x == 2) && (y == 1 || y == 2) {
                continue;
            }
            let offset_x = if x >= 2 { filler_width as f32 } else { 0.0 };
            let offset_y = if y >= 2 { filler_width as f32 } else { 0.0 };
            spawn_part(
                &mut commands,
                &parts.square,
                Transform::from_xyz(
                    (x - 2) as f32 * square_width as f32 + offset_x,
                    0.0,
                    (y - 2) as f32 * square_width as f32 + offset_y,
                ),
            );
        }

        let corner =
            Transform::from_xyz(-2.0 * square_width as f32, 0.0, -2.0 * square_width as f32);
        if grid.level == 0 {
            spawn_part(&mut commands, &parts.center, corner);
        } else {
            spawn_part(&mut commands, &parts.filler, corner);
            spawn_part(
                &mut commands,
                &parts.stitch,
                Transform::from_xyz(-square_width as f32, 0.0, -square_width as f32)
                    .with_scale(Vec3::splat(0.5)),
            );
        }

        grid.trim = spawn_part(&mut commands, &parts.trim, corner);
    }
}

/// Per-frame: snap each LOD grid (and its trim) to the target's toroidal grid.
/// The RVT samples by world position, so nothing per-material updates here.
pub(crate) fn update_grids(
    mut transforms: Query<&mut Transform>,
    clipmaps: Query<&Clipmap>,
    grids: Query<(Entity, &ClipmapGrid, &ChildOf), With<Transform>>,
) {
    for (entity, grid, child_of) in grids {
        // A grid can outlive the entities it references (parent clipmap, its
        // `target`, or `trim`) for a frame during despawn. Skip it rather than panic.
        let Ok(clipmap) = clipmaps.get(child_of.parent()) else {
            continue;
        };
        let filler_width = 2 - clipmap.half_width as i32 % 2;
        let snap_scale = grid.scale(clipmap.base_scale) * filler_width as f32;
        let Ok(target_pos) = transforms.get(clipmap.target).map(|t| t.translation) else {
            continue;
        };
        let snap_factor = (target_pos / snap_scale).floor().as_ivec3().xz();
        let snap_pos = snap_factor.as_vec2() * snap_scale;
        // Snap positions only change when the camera crosses a `snap_scale`
        // boundary. Writing unconditionally marks every grid + trim `Transform`
        // changed each frame, forcing GlobalTransform propagation and render
        // re-extraction over the whole terrain subtree while the camera is still.
        // Guard each write so an idle frame dirties nothing.
        let grid_pos = snap_pos.extend(0.0).xzy();
        if let Ok(mut grid_transform) = transforms.get_mut(entity)
            && grid_transform.translation != grid_pos
        {
            grid_transform.translation = grid_pos;
        }

        let snap_mod2 = ((snap_factor % 2) + 2) % 2;
        let trim_translation = {
            let offset_0 = filler_width as f32 - clipmap.half_width as f32;
            let offset_1 = clipmap.half_width as f32;
            Vec3 {
                x: if snap_mod2.x == 0 { offset_0 } else { offset_1 },
                y: 0.0,
                z: if snap_mod2.y == 0 { offset_0 } else { offset_1 },
            }
        };
        let trim_rotation = Quat::from_rotation_y(match snap_mod2 {
            IVec2 { x: 0, y: 0 } => 0.0,
            IVec2 { x: 0, y: 1 } => FRAC_PI_2,
            IVec2 { x: 1, y: 0 } => -FRAC_PI_2,
            IVec2 { x: 1, y: 1 } => PI,
            _ => unreachable!(),
        });
        let Ok(mut trim_transform) = transforms.get_mut(grid.trim) else {
            continue;
        };
        if trim_transform.translation != trim_translation {
            trim_transform.translation = trim_translation;
        }
        if trim_transform.rotation != trim_rotation {
            trim_transform.rotation = trim_rotation;
        }
    }
}

/// Applies a changed [`Clipmap::edit_overlay`] to the clipmap's materials
/// (`editing` feature). The materials are built at spawn, before an editor has
/// typically created the overlay texture (it needs the loaded heightmap's
/// dimensions) — this picks up the later assignment.
#[cfg(feature = "editing")]
pub(crate) fn sync_edit_overlay(
    clipmaps: Query<(&Clipmap, &ClipmapMaterials), Changed<Clipmap>>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
) {
    for (clipmap, mats) in &clipmaps {
        let Some(overlay) = &clipmap.edit_overlay else {
            continue;
        };
        for handle in [&mats.solid, &mats.wireframe] {
            // Check before `get_mut`: mutable access alone marks the material
            // modified and rebuilds its bind group.
            if materials
                .get(handle)
                .is_some_and(|m| m.extension.edit_overlay != *overlay)
                && let Some(mut material) = materials.get_mut(handle)
            {
                material.extension.edit_overlay = overlay.clone();
            }
        }
    }
}

/// Terrain sun-visibility at an arbitrary world point — the CPU counterpart of the
/// self-shadow the RVT bakes for the terrain *surface*.
///
/// The baked RVT channel is a 2D function of world XZ, valid only *on* the surface,
/// so it's wrong for a point at altitude. This marches the heightmap from the given
/// 3D point toward the fixed sun instead, correct at any height — the query flying
/// characters need (design doc §4.3).
///
/// O(points marched), not per-pixel: call it **once per entity**, never per
/// fragment; throttle or cache for more headroom.
///
/// ```no_run
/// # use bevy::prelude::*;
/// # use bevy_wilderness::SunVisibility;
/// fn shade_flyers(sun: SunVisibility, flyers: Query<&GlobalTransform>) {
///     for xf in &flyers {
///         if let Some(vis) = sun.sample(xf.translation()) {
///             // vis: 1 = full sun, 0 = fully shadowed by terrain.
///         }
///     }
/// }
/// ```
#[derive(SystemParam)]
pub struct SunVisibility<'w, 's> {
    clipmaps: Query<'w, 's, (&'static Clipmap, &'static ClipmapRvt)>,
    images: Res<'w, Assets<Image>>,
}

impl SunVisibility<'_, '_> {
    /// Sun visibility at `world_pos`: `1.0` = full sun, `0.0` = fully shadowed,
    /// soft penumbra between. Marches from `world_pos` itself, so pass a point above
    /// the surface (an entity's position) — an on-surface point reads a self-shadow.
    ///
    /// `None` if the point is outside every clipmap, the bake hasn't initialized, or
    /// the heightmap isn't CPU-resident (needs the default `MAIN_WORLD` asset usage).
    pub fn sample(&self, world_pos: Vec3) -> Option<f32> {
        for (clipmap, rvt) in &self.clipmaps {
            if !rvt.initialized {
                continue;
            }
            let Some(image) = self.images.get(&clipmap.heightmap) else {
                continue;
            };
            let Some(field) = Heightfield::new(image, clipmap.texel_size, clipmap.min, clipmap.max)
            else {
                continue;
            };
            if !field.contains(world_pos) {
                continue;
            }
            return Some(field.sun_visibility(world_pos, rvt.sun_direction));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::asset::RenderAssetUsages;
    use bevy::render::render_resource::{Extent3d, TextureDimension};

    #[test]
    fn r16uint_heightmap_retags_to_unorm() {
        // Simulate what bevy's PNG decoder produces: R16 bytes tagged R16Uint.
        let mut image = Image::new(
            Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            vec![0; 32],
            TextureFormat::R16Uint,
            RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
        );
        let data_before = image.data.clone();
        assert!(retag_r16uint(&mut image));
        assert_eq!(image.texture_descriptor.format, TextureFormat::R16Unorm);
        assert_eq!(image.data, data_before, "a relabel, not a transcode");
        // Already-Unorm images are left untouched (no spurious re-upload).
        assert!(!retag_r16uint(&mut image));
    }
}
