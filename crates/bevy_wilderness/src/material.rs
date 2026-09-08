use bevy::{
    asset::{embedded_path, AssetPath},
    pbr::MaterialExtension,
    prelude::*,
    render::render_resource::{AsBindGroup, ShaderType},
    shader::ShaderRef,
};

use crate::height_fog::HeightFogParams;
use crate::holes::HoleBuffer;
use crate::DetailConfig;

/// Pipeline-specializing state. `holes` keys a separate pipeline so terrain
/// without holes compiles a vertex shader with no shape loop at all.
#[repr(C)]
#[derive(Eq, PartialEq, Hash, Copy, Clone)]
pub(crate) struct GridMaterialKey {
    wireframe: bool,
    holes: bool,
}

impl From<&GridMaterial> for GridMaterialKey {
    fn from(material: &GridMaterial) -> Self {
        Self {
            wireframe: material.flags & 1 != 0,
            holes: material.dev.hole_count > 0,
        }
    }
}

/// Dev/experiment + editing scalars packed into one uniform so `GridMaterial`
/// stays under the bind-group ceiling (see its banner — the ceiling is uniform
/// binding *slots*, so growing this struct is the sanctioned way to pass new
/// scalars). `ao_strength`/`bent_strength` toggle the two halves of the
/// ambient bake (B / N keys); `debug_view` cycles the channel isolation
/// (V key). The `stamp_*` fields are the D11 stamp preview's transform, read
/// by the vertex shader while flags bit5 is set; zeroed and ignored
/// otherwise.
#[derive(Clone, Copy, Debug, PartialEq, ShaderType, Reflect)]
pub(crate) struct DevParams {
    /// Macro AO strength: 0 off, 1 full.
    pub(crate) ao_strength: f32,
    /// Bent-normal strength: 0 off, 1 full.
    pub(crate) bent_strength: f32,
    /// Debug channel isolation: 0 lit, 1 macro AO, 2 bent normal, 3 cavity.
    pub(crate) debug_view: u32,
    /// Stamp preview center, world XZ meters.
    pub(crate) stamp_center: Vec2,
    /// Stamp preview half extents, world meters (X = width/2, Y = depth/2).
    pub(crate) stamp_half_size: Vec2,
    /// Stamp preview rotation about +Y, radians.
    pub(crate) stamp_rotation: f32,
    /// Stamp preview height contribution at full-white texels, meters
    /// (signed: negative carves).
    pub(crate) stamp_strength: f32,
    /// Terrain holes, packed — see `holes.rs`. Read by the vertex shader only
    /// when `hole_count > 0` (which also selects the pipeline that loops).
    #[reflect(ignore)]
    pub(crate) holes: HoleBuffer,
    pub(crate) hole_count: u32,
}

impl Default for DevParams {
    fn default() -> Self {
        Self {
            ao_strength: 1.0,
            bent_strength: 1.0,
            debug_view: 0,
            stamp_center: Vec2::ZERO,
            stamp_half_size: Vec2::ZERO,
            stamp_rotation: 0.0,
            stamp_strength: 0.0,
            holes: HoleBuffer::default(),
            hole_count: 0,
        }
    }
}

/// Near-range detail-overlay parameters (packed for the GPU).
#[derive(Clone, Copy, Debug, Default, ShaderType, Reflect)]
pub(crate) struct DetailParams {
    tiling: f32,
    normal_strength: f32,
    albedo_strength: f32,
    /// Camera distances (m) over which the overlay fades out.
    near: f32,
    far: f32,
}

impl DetailParams {
    pub(crate) fn from_config(d: &DetailConfig) -> Self {
        Self {
            tiling: d.tiling.max(1e-3),
            normal_strength: d.normal_strength,
            albedo_strength: d.albedo_strength,
            near: d.near,
            far: d.far,
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
#[bind_group_data(GridMaterialKey)]
pub(crate) struct GridMaterial {
    #[texture(102)]
    #[sampler(103)]
    pub(crate) heightmap: Handle<Image>,
    // The three RVT targets share one sampler (122) — all sampled linearly at `uv`.
    #[texture(121)]
    #[sampler(122)]
    pub(crate) rvt_albedo: Handle<Image>,
    #[texture(123)]
    pub(crate) rvt_normal: Handle<Image>,
    #[texture(132)]
    pub(crate) rvt_ao: Handle<Image>,
    /// Dev/experiment scalars (macro-AO / bent-normal strength, debug view) packed
    /// into one uniform — see [`DevParams`]. Frees two binding slots vs. separate
    /// uniforms; the freed 112/113 stay clear as ceiling headroom.
    #[uniform(110)]
    pub(crate) dev: DevParams,
    /// Inline height fog (`Low` tier): `density > 0` fogs in the terrain shader —
    /// free, terrain-only. `disabled()` skips it (`High` uses
    /// [`HeightFogPlugin`](crate::HeightFogPlugin)).
    #[uniform(114)]
    pub(crate) fog: HeightFogParams,
    // The three detail arrays share one sampler (126) — same tiling/aniso config.
    #[texture(125, dimension = "2d_array")]
    #[sampler(126)]
    pub(crate) detail_albedo_array: Handle<Image>,
    #[texture(127, dimension = "2d_array")]
    pub(crate) detail_normal_array: Handle<Image>,
    #[uniform(129)]
    pub(crate) detail: DetailParams,
    #[texture(130, dimension = "2d_array")]
    pub(crate) detail_orm_array: Handle<Image>,
    /// Editor visualization overlay (`editing` feature; see
    /// `Clipmap::edit_overlay`). A **texture** binding sharing the heightmap's
    /// sampler (103): textures have headroom here — it's *uniform buffers* that
    /// sit at the silent-break ceiling (banner above) — and the binding
    /// compiles out of non-editing builds entirely (the shader reads it behind
    /// the def pushed in `specialize`).
    #[cfg(feature = "editing")]
    #[texture(115)]
    pub(crate) edit_overlay: Handle<Image>,
    /// Stamp preview heightfield (`editing`, D11): a grayscale texture the
    /// vertex shader composites under the cursor while flags bit5 is set —
    /// the GPU floating preview. Same ceiling story as `edit_overlay`: a
    /// texture binding sharing the heightmap sampler, compiled out of
    /// non-editing builds behind the `WILDERNESS_STAMP` def.
    #[cfg(feature = "editing")]
    #[texture(116)]
    pub(crate) stamp: Handle<Image>,
    #[uniform(108)]
    pub(crate) texel_size: f32,
    #[uniform(109)]
    pub(crate) minmax: Vec2,
    /// Packed flags: bit0 wireframe, bit1 ambient gather, bit2 single-layer detail,
    /// bit3 looping, bit4 clay display mode (D10, `editing`). Quality knobs ride
    /// here rather than adding uniforms (this material is at the bind-group binding
    /// limit — extra uniforms silently break its pipeline).
    #[uniform(111)]
    pub(crate) flags: u32,
}

impl GridMaterial {
    /// `flags` bit4: clay display mode (D10) — grey + screen-derivative
    /// normals instead of the baked RVT. Kept in sync by `sync_clay_flag`.
    #[cfg(feature = "editing")]
    pub(crate) const FLAG_CLAY: u32 = 1 << 4;
    /// `flags` bit5: stamp preview active (D11) — the vertex shader
    /// composites the stamp texture through the `DevParams::stamp_*`
    /// transform. Kept in sync by `sync_stamp_preview`.
    #[cfg(feature = "editing")]
    pub(crate) const FLAG_STAMP: u32 = 1 << 5;
    /// `flags` bit6: the stamp preview is mask-confined (D11) — its
    /// contribution is weighted by the edit-overlay (mask) texture, matching
    /// what the commit will do.
    #[cfg(feature = "editing")]
    pub(crate) const FLAG_STAMP_MASKED: u32 = 1 << 6;
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

    /// The shadow pass renders casters with the *prepass* vertex shader;
    /// without this override it would use the default mesh vertex and cast the
    /// flat, undisplaced grid. Needed for clay mode's dynamic shadows (D10) —
    /// the terrain only ever casts while clay (`sync_clay_shadows`), but the
    /// pipeline must displace whenever it does. Same shader file: `fn vertex`
    /// already compiles under `PREPASS_PIPELINE` (the deferred path uses it).
    fn prepass_vertex_shader() -> ShaderRef {
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
        // Vertex-stage only: the cut collapses triangles before rasterization,
        // so every pipeline (shadow/prepass included) gets it from `fn vertex`.
        if key.bind_group_data.holes {
            descriptor
                .vertex
                .shader_defs
                .push("WILDERNESS_HOLES".into());
        }
        // terrain.wgsl's fragment machinery only compiles in pipelines that
        // shade: the forward pass and the deferred g-buffer pass. Depth-only
        // pipelines — the shadow pass casting the terrain's displaced
        // silhouette (clay mode, D10) and depth/normal prepasses — compile
        // `fn vertex` alone; `pbr_fragment` doesn't build against their
        // trimmed VertexOutput and their layouts lack the light bindings.
        let has_def = |defs: &[bevy::shader::ShaderDefVal], name: &str| {
            use bevy::shader::ShaderDefVal;
            defs.iter().any(|def| match def {
                ShaderDefVal::Bool(n, _) | ShaderDefVal::Int(n, _) | ShaderDefVal::UInt(n, _) => {
                    n == name
                }
            })
        };
        let defs = &descriptor.vertex.shader_defs;
        if !has_def(defs, "PREPASS_PIPELINE") || has_def(defs, "DEFERRED_PREPASS") {
            descriptor
                .vertex
                .shader_defs
                .push("WILDERNESS_SHADE".into());
            if let Some(fragment) = descriptor.fragment.as_mut() {
                fragment.shader_defs.push("WILDERNESS_SHADE".into());
            }
        }
        // The edit-overlay and stamp bindings exist only in editing builds;
        // gate the shader's declarations + samples on defs so the same
        // terrain.wgsl compiles against both bind-group layouts. Both go to
        // the *vertex* stage too: the stamp preview composites in the vertex
        // shader (including depth-only shadow pipelines, so a previewed
        // mountain casts its previewed silhouette) and samples the overlay
        // for its mask weight.
        #[cfg(feature = "editing")]
        {
            let vertex = &mut descriptor.vertex.shader_defs;
            vertex.push("WILDERNESS_EDIT_OVERLAY".into());
            vertex.push("WILDERNESS_STAMP".into());
            if let Some(fragment) = descriptor.fragment.as_mut() {
                fragment.shader_defs.push("WILDERNESS_EDIT_OVERLAY".into());
                fragment.shader_defs.push("WILDERNESS_STAMP".into());
            }
        }
        Ok(())
    }
}
