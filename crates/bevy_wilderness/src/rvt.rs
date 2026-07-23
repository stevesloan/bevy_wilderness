use bevy::{
    asset::{embedded_path, AssetPath},
    camera::{visibility::RenderLayers, RenderTarget, ScalingMode},
    core_pipeline::tonemapping::Tonemapping,
    pbr::Material,
    prelude::*,
    render::{
        gpu_readback::{Readback, ReadbackComplete},
        render_resource::{AsBindGroup, ShaderType, TextureFormat, TextureUsages},
    },
    shader::ShaderRef,
};

#[cfg(feature = "editing")]
use crate::RebakeRequested;
use crate::{Clipmap, ClipmapReady, TerrainQuality, MAX_TERRAIN_LAYERS};

/// Render layers isolating the RVT bake cameras/quads from the main view.
const RVT_ALBEDO_LAYER: usize = 1;
const RVT_NORMAL_LAYER: usize = 2;
/// Macro AO + bent normal + cavity gather target (bake mode 2).
const RVT_AO_LAYER: usize = 3;
/// Layer offset for the tiny sentinel targets that detect bake readiness. Must
/// exceed the number of bake targets so sentinel layers can't collide with a
/// target's base layer.
const RVT_SENTINEL_LAYER_OFFSET: usize = 3;

/// Per-layer parameters packed for the GPU. `Vec4` lanes index the layers.
#[derive(Clone, Copy, Debug, Default, ShaderType, Reflect)]
struct TerrainParams {
    tiling_scale: Vec4,
    height_blend: Vec4,
    roughness: Vec4,
    normal_strength: Vec4,
    /// Slope band per layer (radians): appears between `slope_min` and `slope_max`,
    /// ramping over `slope_blend` at each edge. No rule = wide-open band (all slopes).
    slope_min: Vec4,
    slope_max: Vec4,
    slope_blend: Vec4,
    /// World-height band per layer (meters): same shape as the slope band.
    height_min: Vec4,
    height_max: Vec4,
    height_range_blend: Vec4,
    layer_count: u32,
}

impl TerrainParams {
    fn from_clipmap(clipmap: &Clipmap) -> Self {
        let mut tiling_scale = [1.0f32; MAX_TERRAIN_LAYERS];
        let mut height_blend = [0.0f32; MAX_TERRAIN_LAYERS];
        let mut roughness = [1.0f32; MAX_TERRAIN_LAYERS];
        let mut normal_strength = [1.0f32; MAX_TERRAIN_LAYERS];
        // No-rule defaults: a band so wide the ramps never fire (weight factor 1).
        let mut slope_min = [-10.0f32; MAX_TERRAIN_LAYERS];
        let mut slope_max = [10.0f32; MAX_TERRAIN_LAYERS];
        let mut slope_blend = [0.01f32; MAX_TERRAIN_LAYERS];
        let mut height_min = [-1.0e9f32; MAX_TERRAIN_LAYERS];
        let mut height_max = [1.0e9f32; MAX_TERRAIN_LAYERS];
        let mut height_range_blend = [1.0f32; MAX_TERRAIN_LAYERS];
        for (i, layer) in clipmap.layers.iter().take(MAX_TERRAIN_LAYERS).enumerate() {
            tiling_scale[i] = layer.tiling_scale.max(1e-3);
            height_blend[i] = layer.height_blend;
            roughness[i] = layer.roughness;
            normal_strength[i] = layer.normal_strength;
            if let Some(slope) = &layer.slope {
                slope_min[i] = slope.min_deg.to_radians();
                slope_max[i] = slope.max_deg.to_radians();
                slope_blend[i] = slope.blend_deg.to_radians().max(1e-3);
            }
            if let Some(h) = &layer.height {
                height_min[i] = h.min;
                height_max[i] = h.max;
                height_range_blend[i] = h.blend.max(1e-3);
            }
        }
        Self {
            tiling_scale: Vec4::from_array(tiling_scale),
            height_blend: Vec4::from_array(height_blend),
            roughness: Vec4::from_array(roughness),
            normal_strength: Vec4::from_array(normal_strength),
            slope_min: Vec4::from_array(slope_min),
            slope_max: Vec4::from_array(slope_max),
            slope_blend: Vec4::from_array(slope_blend),
            height_min: Vec4::from_array(height_min),
            height_max: Vec4::from_array(height_max),
            height_range_blend: Vec4::from_array(height_range_blend),
            layer_count: clipmap.layers.len().min(MAX_TERRAIN_LAYERS) as u32,
        }
    }
}

/// RVT (runtime virtual texture) state for a clipmap: the baked material texture
/// the main pass samples instead of blending the splat per-fragment.
#[derive(Component)]
pub(crate) struct ClipmapRvt {
    pub(crate) albedo: Handle<Image>,
    pub(crate) normal: Handle<Image>,
    pub(crate) ao: Handle<Image>,
    pub(crate) initialized: bool,
    /// Whether any bake has ever *completed* for this clipmap. Never reset —
    /// unlike `initialized`, which re-arms per re-bake — so it distinguishes
    /// "never shaded" (the D10 clay fallback) from "re-baking with the
    /// previous bake still on screen".
    pub(crate) ever_baked: bool,
    /// Bake targets not yet finished. Set when the bake cameras spawn; each
    /// decrements as it completes, and [`ClipmapReady`] is inserted at zero.
    pub(crate) pending_bakes: u32,
    /// Sun direction the shadow was baked against (from the scene `DirectionalLight`,
    /// resolved in `init_rvt`), so [`SunVisibility`] marches toward the same sun.
    /// `ZERO` until `initialized`.
    pub(crate) sun_direction: Vec3,
    /// The quality profile this clipmap baked with — its bake-time fields are
    /// locked in at spawn, so `warn_late_quality` can flag later changes.
    pub(crate) quality: TerrainQuality,
    /// Seconds since spawn while not yet [`ClipmapReady`]; drives the stall warning.
    pub(crate) stall_secs: f32,
    /// Set once the stall warning has fired, so it warns at most once.
    pub(crate) stall_warned: bool,
}

/// A bake camera stays inactive until its sentinel readback proves the bake
/// pipeline is compiled and source textures are on the GPU (`ready`), then
/// renders a few frames (the full-coverage RVT is static) and deactivates.
/// Pipeline compilation and asset upload take a machine-dependent number of
/// frames; a camera that renders before they finish produces an empty target.
#[derive(Component)]
pub(crate) struct RvtBakeCamera {
    ready: bool,
    frames: u32,
    /// The clipmap entity this camera bakes for, so completion can be tallied.
    clipmap: Entity,
    /// The full-coverage bake quad this camera renders; despawned with the camera
    /// once the bake finishes (it and the camera are dead weight afterward — the
    /// baked target lives on in `ClipmapRvt`/`GridMaterial`).
    quad: Entity,
}

/// Active frames rendered once ready; > 1 only as safety margin.
const RVT_BAKE_FRAMES: u32 = 2;

pub(crate) fn drive_rvt_bake(
    mut commands: Commands,
    mut cameras: Query<(Entity, &mut Camera, &mut RvtBakeCamera)>,
    mut rvts: Query<&mut ClipmapRvt>,
) {
    for (camera_entity, mut camera, mut state) in &mut cameras {
        if !state.ready {
            continue;
        }
        if state.frames > 0 {
            camera.is_active = true;
            state.frames -= 1;
        } else if camera.is_active {
            camera.is_active = false;
            // This target is baked. When the clipmap's last one finishes, mark
            // it ready. Guarded by `is_active`, so this fires exactly once per
            // camera. `try_insert` in case the clipmap despawned this frame.
            if let Ok(mut rvt) = rvts.get_mut(state.clipmap) {
                rvt.pending_bakes = rvt.pending_bakes.saturating_sub(1);
                if rvt.pending_bakes == 0 {
                    rvt.ever_baked = true;
                    commands.entity(state.clipmap).try_insert(ClipmapReady);
                }
            }
            // Free the now-idle bake camera and its full-coverage quad (which
            // pins the source terrain textures via its BakeMaterial). Without
            // this they render/iterate every frame for the app's lifetime.
            commands.entity(state.quad).try_despawn();
            commands.entity(camera_entity).try_despawn();
        }
    }
}

/// Handles [`RebakeRequested`] (`editing` feature): re-arm the clipmap's bake so
/// `init_rvt` re-runs the full sentinel-gated pipeline next frame — same path as
/// the initial bake, so it stays correct as that evolves. The initial bake's
/// teardown already despawns every bake entity (`drive_rvt_bake`), so re-spawning
/// is a clean slate, and the RVT targets keep their old content until the new
/// bake overwrites them (no unbaked chrome-mirror flash).
///
/// If the live [`TerrainQuality`] bake-time fields drifted from the spawn
/// snapshot (an editor's quality panel), the request also applies them: the RVT
/// targets are resized **in place** — same handles, so every material binding
/// stays valid — the material's packed quality bits are re-derived, and the
/// snapshot updated so the bake agrees. Resizing blanks the targets, so the
/// terrain goes clay until the new bake lands rather than sampling garbage.
///
/// Requests are deferred (component kept) while the clipmap hasn't finished its
/// current bake — processing one mid-flight would reset `pending_bakes` under the
/// in-flight cameras and corrupt the tally.
#[cfg(feature = "editing")]
pub(crate) fn process_rebake_requests(
    mut commands: Commands,
    quality: Res<TerrainQuality>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<
        Assets<bevy::pbr::ExtendedMaterial<StandardMaterial, crate::material::GridMaterial>>,
    >,
    mut clipmaps: Query<
        (
            Entity,
            &mut Clipmap,
            &mut ClipmapRvt,
            &crate::clipmap::ClipmapMaterials,
        ),
        With<RebakeRequested>,
    >,
) {
    for (entity, mut clipmap, mut rvt, clipmap_materials) in &mut clipmaps {
        if !rvt.initialized || rvt.pending_bakes > 0 {
            continue;
        }
        let drifted = quality.rvt_size != rvt.quality.rvt_size
            || quality.ambient_gather != rvt.quality.ambient_gather
            || quality.detail_layers != rvt.quality.detail_layers;
        if drifted {
            let mut resize = |handle: &Handle<Image>, size: u32, format: TextureFormat| {
                if let Some(mut image) = images.get_mut(handle) {
                    let mut target = Image::new_target_texture(size, size, format, None);
                    if clipmap.looping {
                        target.sampler = crate::texture::looping_rvt_sampler();
                    }
                    *image = target;
                }
            };
            let size = quality.rvt_size;
            resize(&rvt.albedo, size, TextureFormat::Rgba8UnormSrgb);
            resize(&rvt.normal, size, TextureFormat::Rgba8Unorm);
            // Same stub rule as `init_clipmaps`: gather off = 4×4 placeholder.
            let ao_size = if quality.ambient_gather { size } else { 4 };
            resize(&rvt.ao, ao_size, TextureFormat::Rgba8Unorm);
            // Re-pack the quality bits (bit1 gather, bit2 single-layer detail),
            // preserving the wireframe and looping bits.
            let quality_bits = ((quality.ambient_gather as u32) << 1)
                | (((quality.detail_layers <= 1) as u32) << 2);
            for handle in [&clipmap_materials.solid, &clipmap_materials.wireframe] {
                if let Some(mut material) = materials.get_mut(handle) {
                    material.extension.flags = (material.extension.flags & !0b110) | quality_bits;
                }
            }
            rvt.quality = *quality;
            // The blanked targets have nothing to show; clay until the bake
            // lands (`clear_clay_on_bake` lifts it on `ClipmapReady`).
            clipmap.clay = true;
        }
        rvt.initialized = false;
        rvt.sun_direction = Vec3::ZERO;
        // Re-arm the stall diagnostic for this bake.
        rvt.stall_secs = 0.0;
        rvt.stall_warned = false;
        commands
            .entity(entity)
            .remove::<(RebakeRequested, ClipmapReady)>();
    }
}

/// Seconds a clipmap may go un-baked before the stall warning fires (generous, so a
/// slow cold-start bake — pipeline compile + texture upload — doesn't trip it).
const BAKE_STALL_WARN_SECS: f32 = 30.0;

/// Warns once per clipmap if the RVT bake hasn't finished after a grace period.
/// Without this a missing `DirectionalLight`, an unloaded heightmap, or non-resident
/// source textures leave the terrain an unbaked chrome mirror with no diagnostic.
pub(crate) fn warn_unbaked_terrain(
    time: Res<Time>,
    mut clipmaps: Query<(&Clipmap, &mut ClipmapRvt), Without<ClipmapReady>>,
    suns: Query<(), With<DirectionalLight>>,
    images: Res<Assets<Image>>,
) {
    for (clipmap, mut rvt) in &mut clipmaps {
        if rvt.stall_warned {
            continue;
        }
        rvt.stall_secs += time.delta_secs();
        if rvt.stall_secs < BAKE_STALL_WARN_SECS {
            continue;
        }
        rvt.stall_warned = true;
        let reason = if suns.is_empty() {
            "no DirectionalLight in the scene — the bake needs one for sun-visibility"
        } else if images.get(&clipmap.heightmap).is_none() {
            "the heightmap image hasn't loaded — check the asset path"
        } else {
            "the bake hasn't finished — source terrain textures may not be GPU-resident \
             (or the bake is just slow on this device)"
        };
        warn!(
            "bevy_wilderness: terrain still unbaked after {:.0}s ({reason}); it renders as a \
             chrome mirror until baked",
            rvt.stall_secs
        );
    }
}

/// Warns if a [`TerrainQuality`] bake-time field (`rvt_size` / `ambient_gather` /
/// `detail_layers`) is changed after the terrain has baked — those only apply at
/// spawn (a rebake), so the change silently does nothing. Only `fog` applies live.
pub(crate) fn warn_late_quality(
    quality: Res<TerrainQuality>,
    clipmaps: Query<(Entity, &ClipmapRvt)>,
    #[cfg(feature = "editing")] rebaking: Query<(), With<RebakeRequested>>,
    mut warned: Local<bool>,
) {
    if !quality.is_changed() || *warned {
        return;
    }
    for (entity, rvt) in &clipmaps {
        // A queued rebake will apply the change (`process_rebake_requests`),
        // so it isn't a silent no-op.
        #[cfg(feature = "editing")]
        if rebaking.contains(entity) {
            continue;
        }
        #[cfg(not(feature = "editing"))]
        let _ = entity;
        let baked = &rvt.quality;
        if rvt.initialized
            && (baked.rvt_size != quality.rvt_size
                || baked.ambient_gather != quality.ambient_gather
                || baked.detail_layers != quality.detail_layers)
        {
            *warned = true;
            warn!(
                "bevy_wilderness: a TerrainQuality bake-time field (rvt_size / ambient_gather / \
                 detail_layers) changed after the terrain baked — no effect without a rebake; \
                 only `fog` applies live"
            );
            break;
        }
    }
}

/// Material that bakes the terrain splat into the RVT. Runs the same source data
/// as `GridMaterial` but outputs raw channels unlit; see `bake.wgsl`.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub(crate) struct BakeMaterial {
    #[texture(0)]
    #[sampler(1)]
    heightmap: Handle<Image>,
    #[uniform(2)]
    texel_size: f32,
    #[uniform(3)]
    minmax: Vec2,
    #[texture(4, dimension = "2d_array")]
    #[sampler(5)]
    albedo_array: Handle<Image>,
    #[uniform(8)]
    params: TerrainParams,
    #[texture(9, dimension = "2d_array")]
    #[sampler(10)]
    normal_array: Handle<Image>,
    #[texture(11, dimension = "2d_array")]
    #[sampler(12)]
    orm_array: Handle<Image>,
    #[uniform(13)]
    output_mode: u32,
    #[uniform(14)]
    sun_direction: Vec3,
    // 1 = read the heightmap toroidally so the shadow/AO marches wrap across tile
    // edges (baked shading tiles seamlessly for a looping clipmap); 0 = clamp.
    #[uniform(15)]
    looping: u32,
}

impl Material for BakeMaterial {
    fn fragment_shader() -> ShaderRef {
        ShaderRef::Path(
            AssetPath::from_path_buf(embedded_path!("bake.wgsl")).with_source("embedded"),
        )
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Opaque
    }
}

/// Once the heightmap is loaded, spawn the top-down bake camera and quad that
/// render the splat into the clipmap's RVT texture (full-terrain coverage).
pub(crate) fn init_rvt(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut bake_materials: ResMut<Assets<BakeMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut clipmaps: Query<(Entity, &Clipmap, &mut ClipmapRvt)>,
    suns: Query<&GlobalTransform, With<DirectionalLight>>,
) {
    for (clipmap_entity, clipmap, mut rvt) in &mut clipmaps {
        if rvt.initialized {
            continue;
        }
        // The quality snapshot taken at spawn (`init_clipmaps`), NOT the live
        // resource: `init_clipmaps` sized the AO target and set the shader flag
        // from it, so the bake must agree. Reading the live resource here would,
        // if quality changed during the heightmap-load window, bake (or skip) the
        // AO target out of step with what the material samples — a silent black.
        let quality = rvt.quality;
        let Some(heightmap) = images.get(&clipmap.heightmap) else {
            continue;
        };
        let Some(sun_direction) = suns.iter().next().map(|t| t.back().as_vec3()) else {
            continue;
        };
        let world_size = clipmap.texel_size * heightmap.texture_descriptor.size.width as f32;
        rvt.initialized = true;
        rvt.sun_direction = sun_direction;
        // Albedo + normal/ORM always; the AO/bent-normal/cavity target only when
        // the ambient gather is enabled (see `TerrainQuality`). All must finish
        // before the clipmap is marked ready — kept in sync with the loop below.
        rvt.pending_bakes = if quality.ambient_gather { 3 } else { 2 };

        let mut make_bake = |mode: u32| {
            bake_materials.add(BakeMaterial {
                heightmap: clipmap.heightmap.clone(),
                texel_size: clipmap.texel_size,
                minmax: Vec2::new(clipmap.min, clipmap.max),
                albedo_array: clipmap.albedo_array.clone(),
                params: TerrainParams::from_clipmap(clipmap),
                normal_array: clipmap.normal_array.clone(),
                orm_array: clipmap.orm_array.clone(),
                output_mode: mode,
                sun_direction,
                looping: clipmap.looping as u32,
            })
        };
        let quad = meshes.add(Plane3d::default().mesh().size(world_size, world_size));

        // Shared by the real bake camera and its sentinel; identical projection
        // keeps the two renders interchangeable.
        let projection = || {
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::Fixed {
                    width: world_size,
                    height: world_size,
                },
                near: 0.0,
                far: 20000.0,
                ..OrthographicProjection::default_3d()
            })
        };
        // Up is -Z so the image's texel layout matches the main pass's
        // `world_xz / world_size + 0.5` sampling (u -> +X, v -> +Z).
        let camera_transform =
            Transform::from_xyz(0.0, 10000.0, 0.0).looking_at(Vec3::ZERO, Vec3::NEG_Z);

        // Two bake targets: albedo (mode 0) and normal/ORM (mode 1). Bevy's
        // camera-to-image is single-target, so each is its own quad + camera on
        // its own render layer, rendered before the main view.
        //
        // Each bake camera starts inactive behind a sentinel: the same material
        // rendered to a tiny readback target. The first non-black readback
        // proves the bake pipeline is compiled and the source textures are on
        // the GPU — only then does the expensive full-res bake render (frame
        // counts are machine-dependent and get it wrong either way).
        let mut targets = vec![
            (
                0u32,
                rvt.albedo.clone(),
                TextureFormat::Rgba8UnormSrgb,
                RVT_ALBEDO_LAYER,
                -3isize,
            ),
            (
                1u32,
                rvt.normal.clone(),
                TextureFormat::Rgba8Unorm,
                RVT_NORMAL_LAYER,
                -2isize,
            ),
        ];
        if quality.ambient_gather {
            targets.push((
                2u32,
                rvt.ao.clone(),
                TextureFormat::Rgba8Unorm,
                RVT_AO_LAYER,
                -1isize,
            ));
        }
        for (mode, target, format, layer, order) in targets {
            let material = make_bake(mode);
            let sentinel_layer = layer + RVT_SENTINEL_LAYER_OFFSET;

            // All bake entities are parented to the clipmap so despawning it
            // recursively tears them down (Bevy despawns descendants) — otherwise
            // a clipmap despawned mid-bake leaks its cameras/quads and its sentinel
            // readback keeps copying GPU->CPU every frame forever. The clipmap sits
            // at the world origin (the terrain is origin-centered), so the identity
            // parent transform leaves the origin-placed quad/camera where they are.
            let bake_quad = commands
                .spawn((
                    Mesh3d(quad.clone()),
                    MeshMaterial3d(material.clone()),
                    Transform::default(),
                    RenderLayers::layer(layer),
                    ChildOf(clipmap_entity),
                ))
                .id();
            let bake_camera = commands
                .spawn((
                    Camera3d::default(),
                    Camera {
                        order,
                        clear_color: Color::BLACK.into(),
                        // Inactive until the sentinel readback flips `ready`.
                        is_active: false,
                        ..default()
                    },
                    RenderTarget::Image(target.into()),
                    projection(),
                    Tonemapping::None,
                    Msaa::Off,
                    camera_transform,
                    RenderLayers::layer(layer),
                    ChildOf(clipmap_entity),
                    RvtBakeCamera {
                        ready: false,
                        frames: RVT_BAKE_FRAMES,
                        clipmap: clipmap_entity,
                        quad: bake_quad,
                    },
                ))
                .id();

            // Sentinel: same mesh/material/format, 4x4 target read back each
            // frame. Must match the bake's pipeline key (target format, MSAA,
            // tonemapping) so its first successful draw implies the bake's
            // pipeline is ready too.
            let mut sentinel_image = Image::new_target_texture(4, 4, format, None);
            sentinel_image.texture_descriptor.usage |= TextureUsages::COPY_SRC;
            let sentinel_target = images.add(sentinel_image);
            let sentinel_quad = commands
                .spawn((
                    Mesh3d(quad.clone()),
                    MeshMaterial3d(material),
                    Transform::default(),
                    RenderLayers::layer(sentinel_layer),
                    ChildOf(clipmap_entity),
                ))
                .id();
            let sentinel_camera = commands
                .spawn((
                    Camera3d::default(),
                    Camera {
                        // Well below every bake camera's order so sentinel and
                        // bake orders never tie (all still render before main).
                        order: order - 10,
                        clear_color: Color::BLACK.into(),
                        ..default()
                    },
                    RenderTarget::Image(sentinel_target.clone().into()),
                    projection(),
                    Tonemapping::None,
                    Msaa::Off,
                    camera_transform,
                    RenderLayers::layer(sentinel_layer),
                    ChildOf(clipmap_entity),
                ))
                .id();
            let readback = commands
                .spawn((Readback::texture(sentinel_target), ChildOf(clipmap_entity)))
                .id();
            commands.entity(readback).observe(
                move |event: On<ReadbackComplete>,
                      mut cameras: Query<&mut RvtBakeCamera>,
                      mut commands: Commands| {
                    // Still clear color -> not rendered yet; keep polling.
                    let baked = event
                        .event()
                        .data
                        .chunks(4)
                        .any(|px| px[0] != 0 || px[1] != 0 || px[2] != 0);
                    if !baked {
                        return;
                    }
                    // The readback can fire again before these despawns flush.
                    // `ready` is set synchronously (query writes apply now, unlike
                    // deferred commands), so it latches the teardown to run once —
                    // otherwise the second fire re-despawns and warns.
                    let Ok(mut camera) = cameras.get_mut(bake_camera) else {
                        return;
                    };
                    if camera.ready {
                        return;
                    }
                    camera.ready = true;
                    commands.entity(sentinel_camera).despawn();
                    commands.entity(sentinel_quad).despawn();
                    commands.entity(readback).despawn();
                },
            );
        }
    }
}
