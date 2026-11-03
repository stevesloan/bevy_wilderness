use bevy::{
    asset::{AssetPath, embedded_path},
    camera::{RenderTarget, ScalingMode, visibility::RenderLayers},
    core_pipeline::tonemapping::Tonemapping,
    pbr::Material,
    prelude::*,
    render::{
        gpu_readback::{Readback, ReadbackComplete},
        render_resource::{AsBindGroup, TextureFormat, TextureUsages},
    },
    shader::ShaderRef,
};

#[cfg(feature = "editing")]
use crate::RebakeRequested;
use crate::{Clipmap, ClipmapReady, TerrainQuality};

/// Render layer isolating the sun shadow-ceiling field's bake camera and quad
/// from the main view. See [`crate::TerrainSunShadow`].
const RVT_SUN_SHADOW_LAYER: usize = 4;
/// Layer offset for the tiny sentinel targets that detect bake readiness. Must
/// exceed the number of bake targets so sentinel layers can't collide with a
/// target's base layer.
const RVT_SENTINEL_LAYER_OFFSET: usize = 4;

/// Bake state for a clipmap's sun shadow-ceiling field, the one whole-world
/// target still baked at load. The surface itself is composited per level by
/// the texture clipmap (`vt.rs`).
#[derive(Component)]
pub(crate) struct ClipmapRvt {
    pub(crate) sun_shadow: Handle<Image>,
    pub(crate) initialized: bool,
    /// The shadow field's latest bake has finished.
    pub(crate) shadow_done: bool,
    /// Whether any bake has ever *completed* for this clipmap. Never reset —
    /// unlike `initialized`, which re-arms per re-bake — so it distinguishes
    /// "never shaded" (the clay fallback) from "re-baking with the
    /// previous bake still on screen".
    pub(crate) ever_baked: bool,
    /// Bake targets not yet finished. Set when the bake cameras spawn; each
    /// decrements as it completes, and [`ClipmapReady`] is inserted at zero.
    pub(crate) pending_bakes: u32,
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
    mut sun_shadows: Query<&mut crate::TerrainSunShadow>,
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
            // This target is baked; `vt::mark_clipmap_ready` inserts
            // `ClipmapReady` once the clipmap's surface is composited too.
            // Guarded by `is_active`, so this fires exactly once per camera.
            if let Ok(mut rvt) = rvts.get_mut(state.clipmap) {
                rvt.pending_bakes = rvt.pending_bakes.saturating_sub(1);
                if rvt.pending_bakes == 0 {
                    rvt.shadow_done = true;
                    // Only now is the shadow field drawn rather than undefined.
                    if let Ok(mut sun_shadow) = sun_shadows.get_mut(state.clipmap) {
                        sun_shadow.params.mark_baked();
                    }
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

/// Handles [`RebakeRequested`] (`editing` feature): re-arm the shadow field's bake
/// so `init_rvt` re-runs the sentinel-gated pipeline next frame — same path as
/// the initial bake — and have the texture clipmap recomposite every level. Both
/// keep their old content on screen until the new lands.
///
/// If the live [`TerrainQuality`] bake-time fields drifted from the spawn
/// snapshot (an editor's quality panel), the request also applies them: the
/// shadow field is resized **in place** — same handle, so every material binding
/// stays valid — and the material's packed quality bits are re-derived. Resizing
/// blanks the field, so the ground takes full sun until it rebakes.
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
            &mut crate::TerrainSunShadow,
            &mut crate::vt::ClipmapVt,
        ),
        With<RebakeRequested>,
    >,
) {
    for (entity, clipmap, mut rvt, clipmap_materials, mut sun_shadow, mut vt) in &mut clipmaps {
        if !rvt.initialized || rvt.pending_bakes > 0 {
            continue;
        }
        let drifted = quality.detail_layers != rvt.quality.detail_layers
            || quality.sun_shadow_size != rvt.quality.sun_shadow_size;
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
            let sun_shadow_size = quality.sun_shadow_size.max(1);
            resize(&rvt.sun_shadow, sun_shadow_size, TextureFormat::Rgba16Float);
            // Resizing blanks the target, so the old field is gone: back to full
            // sun until the new bake lands, rather than sampling undefined data.
            sun_shadow.params.invalidate();
            // Re-pack the single-layer detail bit (bit2), preserving the rest.
            let quality_bits = ((quality.detail_layers <= 1) as u32) << 2;
            for handle in [&clipmap_materials.solid, &clipmap_materials.wireframe] {
                if let Some(mut material) = materials.get_mut(handle) {
                    material.extension.flags = (material.extension.flags & !0b100) | quality_bits;
                }
            }
            rvt.quality = *quality;
        }
        rvt.initialized = false;
        rvt.shadow_done = false;
        vt.refresh = true;
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
            "no DirectionalLight in the scene — the bake needs one for the sun-shadow field"
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

/// Warns if a [`TerrainQuality`] bake-time field (`detail_layers` /
/// `sun_shadow_size`) is changed after the terrain has baked — those only apply at
/// spawn (a rebake), so the change silently does nothing. The texture clipmap's
/// fields rebuild it live.
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
            && (baked.detail_layers != quality.detail_layers
                || baked.sun_shadow_size != quality.sun_shadow_size)
        {
            *warned = true;
            warn!(
                "bevy_wilderness: a TerrainQuality bake-time field (detail_layers / \
                 sun_shadow_size) changed after the terrain baked — no effect \
                 without a rebake; only `fog` applies live"
            );
            break;
        }
    }
}

/// Material that bakes the sun shadow-ceiling field from the heightfield; see
/// `bake.wesl`.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub(crate) struct BakeMaterial {
    #[texture(0)]
    #[sampler(1)]
    heightmap: Handle<Image>,
    #[uniform(2)]
    texel_size: f32,
    #[uniform(3)]
    minmax: Vec2,
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
            AssetPath::from_path_buf(embedded_path!("bake.wesl")).with_source("embedded"),
        )
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Opaque
    }
}

/// Once the heightmap is loaded, spawn the top-down bake camera and quad that
/// render the sun shadow-ceiling field (full-terrain coverage).
pub(crate) fn init_rvt(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut bake_materials: ResMut<Assets<BakeMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut clipmaps: Query<(
        Entity,
        &Clipmap,
        &mut ClipmapRvt,
        &mut crate::TerrainSunShadow,
    )>,
    suns: Query<&GlobalTransform, With<DirectionalLight>>,
) {
    for (clipmap_entity, clipmap, mut rvt, mut sun_shadow) in &mut clipmaps {
        if rvt.initialized {
            continue;
        }
        let Some(heightmap) = images.get(&clipmap.heightmap) else {
            continue;
        };
        let Some(sun_direction) = suns.iter().next().map(|t| t.back().as_vec3()) else {
            continue;
        };
        let world_size = clipmap.texel_size * heightmap.texture_descriptor.size.width as f32;
        // A re-bake keeps the previous field on screen until the new one lands
        // (same as the other RVT targets), so validity carries over — unless the
        // target was blanked by a resize, which clears it in the rebake path.
        let was_valid = sun_shadow.params.valid;
        sun_shadow.params = crate::SunShadowParams::pending(world_size, clipmap.min);
        sun_shadow.params.valid = was_valid;
        rvt.initialized = true;
        // Kept in sync with the targets below.
        rvt.pending_bakes = 1;

        let mut make_bake = || {
            bake_materials.add(BakeMaterial {
                heightmap: clipmap.heightmap.clone(),
                texel_size: clipmap.texel_size,
                minmax: Vec2::new(clipmap.min, clipmap.max),
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

        // The bake target gets its own quad and camera on its own render layer,
        // rendered before the main view.
        //
        // Each bake camera starts inactive behind a sentinel: the same material
        // rendered to a tiny readback target. The first non-black readback
        // proves the bake pipeline is compiled and the source textures are on
        // the GPU — only then does the expensive full-res bake render (frame
        // counts are machine-dependent and get it wrong either way).
        let targets = [(
            rvt.sun_shadow.clone(),
            TextureFormat::Rgba16Float,
            RVT_SUN_SHADOW_LAYER,
            -4isize,
        )];
        for (target, format, layer, order) in targets {
            let material = make_bake();
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
