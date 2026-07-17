use std::f32::consts::{FRAC_PI_2, PI};

#[cfg(feature = "dev-controls")]
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::{
    asset::{AssetPath, embedded_asset, embedded_path},
    camera::{primitives::Aabb, visibility::NoAutoAabb},
    ecs::system::SystemParam,
    light::NotShadowCaster,
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
    render::render_resource::{AsBindGroup, ShaderType, TextureFormat},
    shader::{ShaderRef, load_shader_library},
};

mod height_fog;
mod heightfield;
mod mesh;
mod mesh_fog;
mod rvt;
mod texture;
pub use height_fog::{HeightFog, HeightFogParams, HeightFogPlugin};
/// Public under `editing` so editor crates reuse the world↔texel + bilinear math
/// that must stay in sync with the shaders (design doc §5.3).
#[cfg(feature = "editing")]
pub use heightfield::Heightfield;
#[cfg(not(feature = "editing"))]
use heightfield::Heightfield;
use mesh::{ClipmapPart, ClipmapParts, build_clipmap_parts};
pub use mesh_fog::HeightFogExtension;
use rvt::{
    BakeMaterial, ClipmapRvt, drive_rvt_bake, init_rvt, warn_late_quality, warn_unbaked_terrain,
};
use texture::looping_rvt_sampler;
pub use texture::{build_terrain_array, load_terrain_array};

pub struct ClipmapPlugin;

impl Plugin for ClipmapPlugin {
    fn build(&self, app: &mut App) {
        // Shared fog math, imported by terrain.wgsl (inline VR fog) and the
        // height_fog.wgsl post-process (flatscreen fog).
        load_shader_library!(app, "fog_functions.wgsl");
        embedded_asset!(app, "terrain.wgsl");
        embedded_asset!(app, "bake.wgsl");
        embedded_asset!(app, "mesh_fog.wgsl");

        app.add_plugins(MaterialPlugin::<
            ExtendedMaterial<StandardMaterial, GridMaterial>,
        >::default())
            .add_plugins(MaterialPlugin::<BakeMaterial>::default())
            .add_plugins(MaterialPlugin::<
                ExtendedMaterial<StandardMaterial, HeightFogExtension>,
            >::default())
            .init_resource::<TerrainFog>()
            .init_resource::<TerrainQuality>()
            .init_resource::<InlineFog>()
            .add_systems(PreUpdate, (init_clipmaps, init_grids))
            .add_systems(
                Update,
                (
                    update_grids,
                    init_rvt,
                    drive_rvt_bake,
                    apply_terrain_quality,
                    fog_new_mesh_materials,
                    warn_unbaked_terrain,
                    warn_late_quality,
                ),
            );

        // Editable-terrain API (design doc §5): consume RebakeRequested before
        // init_rvt so a re-armed bake re-spawns its cameras the same frame.
        #[cfg(feature = "editing")]
        app.add_systems(Update, rvt::process_rebake_requests.before(init_rvt));

        // Demo A/B keybinds for the AO/bent-normal experiment (B/N/V). Off by
        // default so the library ships no input systems; enable `dev-controls`.
        #[cfg(feature = "dev-controls")]
        app.add_systems(Update, (debug_cycle_view, toggle_ao, toggle_bent));
    }
}

/// Maximum number of terrain material layers, blended per pixel by their
/// procedural slope/height weights (§3.2). Bounded by the `Vec4` lanes in
/// [`TerrainParams`]; widen those to raise it.
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

/// Near-range detail-overlay parameters (packed for the GPU).
/// Dev/experiment scalars packed into one uniform so `GridMaterial` stays under the
/// bind-group ceiling (see its banner). `ao_strength`/`bent_strength` toggle the two
/// halves of the ambient bake (B / N keys); `debug_view` cycles the channel isolation
/// (V key). All experiment-only — normal renders leave these at their defaults.
#[derive(Clone, Copy, Debug, ShaderType, Reflect)]
struct DevParams {
    /// Macro AO strength: 0 off, 1 full.
    ao_strength: f32,
    /// Bent-normal strength: 0 off, 1 full.
    bent_strength: f32,
    /// Debug channel isolation: 0 lit, 1 macro AO, 2 bent normal, 3 cavity.
    debug_view: u32,
}

impl Default for DevParams {
    fn default() -> Self {
        Self {
            ao_strength: 1.0,
            bent_strength: 1.0,
            debug_view: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, ShaderType, Reflect)]
struct DetailParams {
    tiling: f32,
    normal_strength: f32,
    albedo_strength: f32,
    /// Camera distances (m) over which the overlay fades out.
    near: f32,
    far: f32,
}

impl DetailParams {
    fn from_config(d: &DetailConfig) -> Self {
        Self {
            tiling: d.tiling.max(1e-3),
            normal_strength: d.normal_strength,
            albedo_strength: d.albedo_strength,
            near: d.near,
            far: d.far,
        }
    }
}

/// The component defining a clipmap.
/// https://hhoppe.com/gpugcm.pdf
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
}

#[derive(Component)]
struct ClipmapGrid {
    level: u32,
    trim: Entity,
}

impl ClipmapGrid {
    fn scale(&self, base_scale: f32) -> f32 {
        base_scale * 2u32.pow(self.level) as f32
    }
}

fn init_clipmaps(
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

fn init_grids(
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
fn update_grids(
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

#[repr(C)]
#[derive(Eq, PartialEq, Hash, Copy, Clone)]
struct WireframeKey {
    wireframe: bool,
}

impl From<&GridMaterial> for WireframeKey {
    fn from(material: &GridMaterial) -> Self {
        Self {
            wireframe: material.flags & 1 != 0,
        }
    }
}

/// Terrain material (extends `StandardMaterial`, sharing its group-2 bindings).
///
/// ⚠️ At the per-stage bind-group ceilings — uniform buffers (adding two once broke
/// the pipeline **silently**: no error, terrain just stopped drawing) and the ~16
/// sampled-texture limit on mobile/VR GPUs. **Don't add bindings.** Reuse one: pack
/// scalars into the `flags` u32 (bit flags) or a spare `vec4`, and share a sampler
/// rather than adding one (the RVT and detail textures each share a single sampler
/// below). Quality knobs ride in `flags` for exactly this reason.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
#[bind_group_data(WireframeKey)]
struct GridMaterial {
    #[texture(102)]
    #[sampler(103)]
    heightmap: Handle<Image>,
    // The three RVT targets share one sampler (122) — all sampled linearly at `uv`.
    #[texture(121)]
    #[sampler(122)]
    rvt_albedo: Handle<Image>,
    #[texture(123)]
    rvt_normal: Handle<Image>,
    #[texture(132)]
    rvt_ao: Handle<Image>,
    /// Dev/experiment scalars (macro-AO / bent-normal strength, debug view) packed
    /// into one uniform — see [`DevParams`]. Frees two binding slots vs. separate
    /// uniforms; the freed 112/113 stay clear as ceiling headroom.
    #[uniform(110)]
    dev: DevParams,
    /// Inline height fog (`Low` tier): `density > 0` fogs in the terrain shader —
    /// free, terrain-only. `disabled()` skips it (`High` uses `HeightFogPlugin`).
    #[uniform(114)]
    fog: HeightFogParams,
    // The three detail arrays share one sampler (126) — same tiling/aniso config.
    #[texture(125, dimension = "2d_array")]
    #[sampler(126)]
    detail_albedo_array: Handle<Image>,
    #[texture(127, dimension = "2d_array")]
    detail_normal_array: Handle<Image>,
    #[uniform(129)]
    detail: DetailParams,
    #[texture(130, dimension = "2d_array")]
    detail_orm_array: Handle<Image>,
    #[uniform(108)]
    texel_size: f32,
    #[uniform(109)]
    minmax: Vec2,
    /// Packed flags: bit0 wireframe, bit1 ambient gather, bit2 single-layer detail.
    /// Quality knobs ride here rather than adding uniforms (this material is at the
    /// bind-group binding limit — extra uniforms silently break its pipeline).
    #[uniform(111)]
    flags: u32,
}

impl MaterialExtension for GridMaterial {
    fn vertex_shader() -> ShaderRef {
        ShaderRef::Path(
            AssetPath::from_path_buf(embedded_path!("terrain.wgsl")).with_source("embedded"),
        )
    }

    fn deferred_vertex_shader() -> ShaderRef {
        ShaderRef::Path(
            AssetPath::from_path_buf(embedded_path!("terrain.wgsl")).with_source("embedded"),
        )
    }

    fn fragment_shader() -> ShaderRef {
        ShaderRef::Path(
            AssetPath::from_path_buf(embedded_path!("terrain.wgsl")).with_source("embedded"),
        )
    }

    fn deferred_fragment_shader() -> ShaderRef {
        ShaderRef::Path(
            AssetPath::from_path_buf(embedded_path!("terrain.wgsl")).with_source("embedded"),
        )
    }

    fn specialize(
        _: &bevy::pbr::MaterialExtensionPipeline,
        descriptor: &mut bevy::render::render_resource::RenderPipelineDescriptor,
        _: &bevy::mesh::MeshVertexBufferLayoutRef,
        key: bevy::pbr::MaterialExtensionKey<Self>,
    ) -> std::result::Result<(), bevy::render::render_resource::SpecializedMeshPipelineError> {
        if key.bind_group_data.wireframe {
            descriptor.primitive.polygon_mode = bevy::render::render_resource::PolygonMode::Line;
            descriptor.depth_stencil.as_mut().unwrap().bias.slope_scale = 1.0;
        }
        Ok(())
    }
}

/// The clipmap's terrain material, solid + wireframe. Identical across all LOD
/// levels (nothing per-level survives in `GridMaterial`), so it's built once per
/// clipmap and shared by every grid, not rebuilt per level.
#[derive(Component)]
struct ClipmapMaterials {
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

/// Press V to cycle the terrain debug view: lit → macro AO → bent normal →
/// cavity → lit. Renders the raw baked RVT-AO channel unlit so it reads as a
/// literal value. Experiment-only inspection aid for the AO/bent-normal bake.
#[cfg(feature = "dev-controls")]
fn debug_cycle_view(
    keys: Res<ButtonInput<KeyCode>>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
    clipmaps: Query<&Clipmap>,
    mut commands: Commands,
) {
    if !keys.just_pressed(KeyCode::KeyV) {
        return;
    }
    let mut next = 0u32;
    let mut computed = false;
    for (_, material) in materials.iter_mut() {
        if !computed {
            next = (material.extension.dev.debug_view + 1) % 4;
            computed = true;
        }
        material.extension.dev.debug_view = next;
    }
    // The debug channels output raw 0..1 values; bypass the filmic tonemapper
    // while one is active so they read faithfully (AO ~0.9 shows near-white, not
    // gray-compressed). Restore the default tonemapper for the lit view.
    let tonemapping = if next == 0 {
        Tonemapping::default()
    } else {
        Tonemapping::None
    };
    for clipmap in &clipmaps {
        commands.entity(clipmap.target).insert(tonemapping);
    }
    let name = match next {
        1 => "macro AO",
        2 => "bent normal",
        3 => "cavity",
        _ => "off (lit terrain)",
    };
    info!("terrain debug view: {name}");
}

/// Press B to toggle the macro AO on/off in the lit render (flips `ao_strength`
/// 1 ↔ 0), so its contribution can be A/B'd on its own.
#[cfg(feature = "dev-controls")]
fn toggle_ao(
    keys: Res<ButtonInput<KeyCode>>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
) {
    if !keys.just_pressed(KeyCode::KeyB) {
        return;
    }
    let mut next = 1.0f32;
    let mut computed = false;
    for (_, material) in materials.iter_mut() {
        if !computed {
            next = if material.extension.dev.ao_strength > 0.5 {
                0.0
            } else {
                1.0
            };
            computed = true;
        }
        material.extension.dev.ao_strength = next;
    }
    info!(
        "terrain macro AO: {}",
        if next > 0.5 { "on" } else { "off" }
    );
}

/// Press N to toggle the bent-normal ambient direction on/off (flips
/// `bent_strength` 1 ↔ 0), independently of the macro AO.
#[cfg(feature = "dev-controls")]
fn toggle_bent(
    keys: Res<ButtonInput<KeyCode>>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
) {
    if !keys.just_pressed(KeyCode::KeyN) {
        return;
    }
    let mut next = 1.0f32;
    let mut computed = false;
    for (_, material) in materials.iter_mut() {
        if !computed {
            next = if material.extension.dev.bent_strength > 0.5 {
                0.0
            } else {
                1.0
            };
            computed = true;
        }
        material.extension.dev.bent_strength = next;
    }
    info!(
        "terrain bent-normal ambient: {}",
        if next > 0.5 { "on" } else { "off" }
    );
}

/// Authored height-fog parameters — how the fog *looks* (an art knob, separate
/// from the [`TerrainQuality`] performance profile). The crate realizes these
/// inline in the terrain (`FogTier::Low`) or via the fullscreen [`HeightFogPlugin`]
/// post-process (`FogTier::High`); both paths stay in sync. `HeightFog::default()
/// .density > 0`, so fog is on by default — set `density: 0.0` to disable.
#[derive(Resource, Clone, Default)]
pub struct TerrainFog(pub HeightFog);

/// How the fog is rendered (a field of [`TerrainQuality`]). Live-switchable —
/// both paths are always compiled.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FogTier {
    /// Fullscreen fog post-process (fogs the sky too, no seam) at the cost of one
    /// framebuffer pass. **Requires `Msaa::Off`** on the target camera — that pass
    /// samples single-sample depth. The crate doesn't set MSAA (that's the game's);
    /// if MSAA is left on, the fog pass skips itself and warns. Desktop budget.
    #[default]
    High,
    /// Inline terrain fog (virtually free, terrain-only, no extra pass). MSAA-friendly
    /// — the game picks the AA level. Tiled GPUs punish fullscreen passes, and
    /// standalone VR wants stable MSAA, so this trades sky-fog for that freedom.
    Low,
}

/// Terrain performance profile — set **once at startup** from device detection
/// (dial the knobs down for a standalone headset, up for desktop). Only
/// [`fog`](Self::fog) applies live; the bake-time fields
/// (`rvt_size`, `ambient_gather`, `detail_layers`) are read when a clipmap bakes —
/// changing them after has no effect (it would need a rebake). [`default`]
/// (Self::default) is desktop-grade.
#[derive(Resource, Clone, Copy, Debug)]
pub struct TerrainQuality {
    /// Fog method (live-switchable). See [`FogTier`].
    pub fog: FogTier,
    /// RVT bake resolution (square). The dominant VRAM cost — three targets of
    /// `size²·4` bytes each (8192² ≈ 768 MB total; 4096² ≈ 192 MB).
    pub rvt_size: u32,
    /// Bake + sample the macro-AO / bent-normal / cavity channel. Off drops a
    /// whole RVT target (VRAM + a slow bake gather) and a per-fragment sample; the
    /// effect is subtle on open terrain, so it's the first thing to cut for VR.
    pub ambient_gather: bool,
    /// Near-detail overlay: blend the top `1` (cheapest, ~3 fewer samples) or `2`
    /// (smoothest boundaries) materials per fragment.
    pub detail_layers: u8,
}

impl Default for TerrainQuality {
    /// Desktop-grade: fullscreen fog, 8192² RVT, ambient gather, top-2 detail.
    /// Dial these down for standalone VR / low-end (e.g. `FogTier::Low`, 2048²
    /// RVT, no ambient gather, single-layer detail).
    fn default() -> Self {
        Self {
            fog: FogTier::High,
            rvt_size: 8192,
            ambient_gather: true,
            detail_layers: 2,
        }
    }
}

/// The active tier's inline fog params — apply these to fog **your own** opaque
/// materials on fast per-material uniforms (no shared buffer / SSBO cost on tiled
/// VR GPUs). The crate's terrain + [`HeightFogExtension`] use it automatically.
///
/// - **Opaque** (buildings, characters): `#import bevy_wilderness::fog_functions`,
///   embed a `#[uniform(N)] HeightFogParams`, copy this in on `.is_changed()`.
///   It's density-0 on `High` (the fullscreen pass fogs opaques there), so it's
///   correct on both tiers.
/// - **Transparent** (particles, explosions): the fullscreen pass can't fog them,
///   so fog on *both* tiers from [`TerrainFog`] (`HeightFogParams::from(&fog.0)`).
#[derive(Resource, Clone, Default)]
pub struct InlineFog(pub HeightFogParams);

/// Inline-terrain fog params for the active tier: the authored fog with density
/// gated to 0 outside the `Low` tier (`High` uses the fullscreen pass instead).
fn inline_fog_params(fog: &TerrainFog, tier: FogTier) -> HeightFogParams {
    let density = if tier == FogTier::Low {
        fog.0.density
    } else {
        0.0
    };
    HeightFogParams::from(&fog.0).with_density(density)
}

/// Realizes [`TerrainFog`] across both fog paths for the active [`TerrainQuality::fog`]
/// tier whenever either changes: writes the inline params into every terrain
/// material, and (if the target camera has a [`HeightFog`], i.e. the fullscreen
/// path is installed) drives its density to match the tier. MSAA is left to the
/// game — the fullscreen fog pass requires `Msaa::Off` and self-skips otherwise.
fn apply_terrain_quality(
    quality: Res<TerrainQuality>,
    fog: Res<TerrainFog>,
    mut inline_fog: ResMut<InlineFog>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
    mut mesh_materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, HeightFogExtension>>>,
    clipmaps: Query<&Clipmap>,
    mut cameras: Query<&mut HeightFog>,
) {
    if !quality.is_changed() && !fog.is_changed() {
        return;
    }
    let low = quality.fog == FogTier::Low;
    let inline = inline_fog_params(&fog, quality.fog);
    // Publish for the game to fog its own materials (change-detected).
    inline_fog.0 = inline.clone();
    for (_, material) in materials.iter_mut() {
        material.extension.fog = inline.clone();
    }
    // Meshes (characters/props) using HeightFogExtension get the same inline fog,
    // so they don't render as unfogged cutouts on the Low tier.
    for (_, material) in mesh_materials.iter_mut() {
        material.extension.fog = inline.clone();
    }
    for clipmap in &clipmaps {
        if let Ok(mut camera_fog) = cameras.get_mut(clipmap.target) {
            *camera_fog = fog.0.clone();
            camera_fog.density = if low { 0.0 } else { fog.0.density };
        }
    }
}

/// Fog mesh materials the moment they're created, so a character/prop spawned at
/// runtime picks up the tier's fog immediately (else an unfogged cutout on `Low`
/// until the next tier change). Touches only new materials — free at steady state.
fn fog_new_mesh_materials(
    mut events: MessageReader<AssetEvent<ExtendedMaterial<StandardMaterial, HeightFogExtension>>>,
    quality: Res<TerrainQuality>,
    fog: Res<TerrainFog>,
    mut mesh_materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, HeightFogExtension>>>,
) {
    let inline = inline_fog_params(&fog, quality.fog);
    for event in events.read() {
        if let AssetEvent::Added { id } = event {
            if let Some(mut material) = mesh_materials.get_mut(*id) {
                material.extension.fog = inline.clone();
            }
        }
    }
}
