//! Fullscreen height-fog post-process — the *flatscreen* fog tier. Fogs the whole
//! view (terrain AND atmosphere sky), so valley mist has no terrain/sky seam;
//! costs one fullscreen pass. The *VR* tier applies the same fog inline in
//! `terrain.wgsl` (terrain-only, virtually free); both share `fog_functions.wgsl`
//! and [`HeightFogParams`], so the tiers match.
//!
//! Add [`HeightFogPlugin`] and put a [`HeightFog`] on an HDR camera. The pass binds
//! single-sampled depth, so it requires `Msaa::Off` on that camera — with MSAA on
//! it skips itself and warns (the crate never overrides the game's MSAA). Runs in
//! `EarlyPostProcess`, in the exposure-applied HDR buffer, before bloom/tonemapping.

use bevy::{
    asset::{AssetServer, Handle, embedded_asset, load_embedded_asset},
    camera::{Camera, Camera3d},
    color::{Color, ColorToComponents},
    core_pipeline::{
        FullscreenShader,
        schedule::{Core3d, Core3dSystems},
    },
    ecs::{
        component::Component,
        entity::Entity,
        query::{QueryItem, With},
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, Res, ResMut},
    },
    log::warn_once,
    math::Vec3,
    prelude::{App, Plugin},
    reflect::Reflect,
    render::{
        GpuResourceAppExt, Render, RenderApp, RenderStartup, RenderSystems,
        extract_component::{
            ComponentUniforms, ExtractComponent, ExtractComponentPlugin, UniformComponentPlugin,
        },
        render_resource::{
            BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries,
            CachedRenderPipelineId, ColorTargetState, ColorWrites, FilterMode, FragmentState,
            LoadOp, Operations, PipelineCache, RenderPassColorAttachment, RenderPassDescriptor,
            RenderPipelineDescriptor, Sampler, SamplerBindingType, SamplerDescriptor, ShaderStages,
            ShaderType, SpecializedRenderPipeline, SpecializedRenderPipelines, StoreOp,
            TextureFormat, TextureSampleType, TextureUsages,
            binding_types::{sampler, texture_2d, uniform_buffer, uniform_buffer_sized},
        },
        renderer::{RenderContext, RenderDevice, ViewQuery},
        sync_component::SyncComponent,
        view::{
            ExtractedView, ViewDepthTexture, ViewTarget, ViewUniform, ViewUniformOffset,
            ViewUniforms, prepare_view_targets,
        },
    },
    shader::Shader,
};

/// Height-fog parameters. Put on an HDR camera for the fullscreen tier; the same
/// values drive the inline terrain tier via [`HeightFogParams`].
#[derive(Component, Clone)]
pub struct HeightFog {
    /// Mist color, linear, in the exposure-applied HDR range (~0..1).
    pub color: Color,
    /// Base density at `base_height`. 0 disables the effect.
    pub density: f32,
    /// World-Y the mist layer sits at.
    pub base_height: f32,
    /// 1/m: how fast density drops with altitude (bigger = a thinner layer that
    /// pools in deep valleys; smaller = mist climbs higher up the slopes).
    pub falloff: f32,
    /// Distance at which fog saturates; also the distance used for sky pixels.
    pub max_distance: f32,
}

impl Default for HeightFog {
    fn default() -> Self {
        Self {
            color: Color::linear_rgb(0.5, 0.58, 0.7),
            density: 0.0008,
            base_height: 0.0,
            falloff: 0.0128,
            max_distance: 30000.0,
        }
    }
}

/// GPU form of [`HeightFog`], matching the WGSL `HeightFog`. Bound both as this
/// post-process's uniform and as a `GridMaterial` binding for the inline terrain
/// fog, so both tiers read identical parameters.
#[derive(Component, ShaderType, Clone, Reflect, Debug, Default)]
pub struct HeightFogParams {
    color: Vec3,
    density: f32,
    base_height: f32,
    falloff: f32,
    max_distance: f32,
    _pad: f32,
}

impl HeightFogParams {
    /// Override density (i.e. which fog tier is active) on an existing set of
    /// params, keeping color/falloff/base/max.
    pub(crate) fn with_density(mut self, density: f32) -> Self {
        self.density = density;
        self
    }

    /// Disabled fog (density 0) — the terrain shader skips the effect entirely.
    pub fn disabled() -> Self {
        Self {
            color: Vec3::ZERO,
            density: 0.0,
            base_height: 0.0,
            falloff: 0.0,
            max_distance: 1.0,
            _pad: 0.0,
        }
    }
}

impl From<&HeightFog> for HeightFogParams {
    fn from(fog: &HeightFog) -> Self {
        Self {
            color: fog.color.to_linear().to_vec3(),
            density: fog.density,
            base_height: fog.base_height,
            falloff: fog.falloff,
            max_distance: fog.max_distance,
            _pad: 0.0,
        }
    }
}

impl SyncComponent for HeightFog {
    type Target = HeightFogParams;
}

impl ExtractComponent for HeightFog {
    type QueryData = &'static Self;
    type QueryFilter = With<Camera>;
    type Out = HeightFogParams;

    fn extract_component(item: QueryItem<Self::QueryData>) -> Option<Self::Out> {
        Some(HeightFogParams::from(item))
    }
}

/// Adds the fullscreen height-fog post-process (the flatscreen fog tier).
pub struct HeightFogPlugin;

impl Plugin for HeightFogPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "height_fog.wgsl");
        app.add_plugins((
            ExtractComponentPlugin::<HeightFog>::default(),
            UniformComponentPlugin::<HeightFogParams>::default(),
        ));

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .init_gpu_resource::<SpecializedRenderPipelines<HeightFogPipeline>>()
            .add_systems(RenderStartup, init_height_fog_pipeline)
            .add_systems(
                Render,
                (
                    prepare_height_fog_pipelines.in_set(RenderSystems::Prepare),
                    prepare_depth_usage
                        .in_set(RenderSystems::PrepareViews)
                        .after(prepare_view_targets)
                        .ambiguous_with_all(),
                ),
            )
            .add_systems(Core3d, height_fog.in_set(Core3dSystems::EarlyPostProcess));
    }
}

#[derive(Resource)]
struct HeightFogPipeline {
    sampler: Sampler,
    layout: BindGroupLayoutDescriptor,
    fullscreen_shader: FullscreenShader,
    fragment_shader: Handle<Shader>,
}

fn init_height_fog_pipeline(
    mut commands: Commands,
    render_device: Res<RenderDevice>,
    fullscreen_shader: Res<FullscreenShader>,
    asset_server: Res<AssetServer>,
) {
    let entries = &BindGroupLayoutEntries::sequential(
        ShaderStages::FRAGMENT,
        (
            // View uniform (dynamic) — for world-position reconstruction.
            uniform_buffer::<ViewUniform>(true),
            // Depth (single-sampled; Msaa must be Off).
            texture_2d(TextureSampleType::Float { filterable: false }),
            // Scene color (read).
            texture_2d(TextureSampleType::Float { filterable: true }),
            // Linear sampler.
            sampler(SamplerBindingType::Filtering),
            // Fog settings.
            uniform_buffer_sized(false, Some(HeightFogParams::min_size())),
        ),
    );
    let sampler = render_device.create_sampler(&SamplerDescriptor {
        mag_filter: FilterMode::Linear,
        min_filter: FilterMode::Linear,
        ..Default::default()
    });
    commands.insert_resource(HeightFogPipeline {
        sampler,
        layout: BindGroupLayoutDescriptor::new("height_fog_layout", entries),
        fullscreen_shader: fullscreen_shader.clone(),
        fragment_shader: load_embedded_asset!(asset_server.as_ref(), "height_fog.wgsl"),
    });
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
struct HeightFogKey {
    target_format: TextureFormat,
}

impl SpecializedRenderPipeline for HeightFogPipeline {
    type Key = HeightFogKey;

    fn specialize(&self, key: Self::Key) -> RenderPipelineDescriptor {
        RenderPipelineDescriptor {
            label: Some("height_fog_pipeline".into()),
            layout: vec![self.layout.clone()],
            vertex: self.fullscreen_shader.to_vertex_state(),
            fragment: Some(FragmentState {
                shader: self.fragment_shader.clone(),
                shader_defs: vec![],
                targets: vec![Some(ColorTargetState {
                    format: key.target_format,
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })],
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

#[derive(Component)]
struct HeightFogPipelineId(CachedRenderPipelineId);

fn prepare_height_fog_pipelines(
    mut commands: Commands,
    pipeline_cache: Res<PipelineCache>,
    mut pipelines: ResMut<SpecializedRenderPipelines<HeightFogPipeline>>,
    pipeline: Res<HeightFogPipeline>,
    views: Query<(Entity, &ExtractedView), With<HeightFogParams>>,
) {
    for (entity, view) in &views {
        let id = pipelines.specialize(
            &pipeline_cache,
            &pipeline,
            HeightFogKey {
                target_format: view.target_format,
            },
        );
        commands.entity(entity).insert(HeightFogPipelineId(id));
    }
}

fn prepare_depth_usage(mut cameras: Query<&mut Camera3d, With<HeightFogParams>>) {
    for mut camera in &mut cameras {
        camera.depth_texture_usages.0 |= TextureUsages::TEXTURE_BINDING.bits();
    }
}

fn height_fog(
    view: ViewQuery<(
        &ViewUniformOffset,
        &ViewTarget,
        &ViewDepthTexture,
        &HeightFogPipelineId,
        &HeightFogParams,
    )>,
    pipeline: Res<HeightFogPipeline>,
    pipeline_cache: Res<PipelineCache>,
    fog_uniforms: Res<ComponentUniforms<HeightFogParams>>,
    view_uniforms: Res<ViewUniforms>,
    mut ctx: RenderContext,
) {
    let (view_offset, view_target, depth, pipeline_id, fog) = view.into_inner();

    if fog.density <= 0.0 {
        return;
    }
    // Depth binds as single-sampled `texture_2d`; MSAA makes it multisampled and the
    // bind fails. MSAA is the game's, so skip our own pass (don't override it) + warn.
    if depth.texture.sample_count() > 1 {
        warn_once!(
            "bevy_wilderness: the fullscreen height-fog pass requires Msaa::Off on the fog \
             camera (its depth binding is single-sampled) — skipping it. Set Msaa::Off, \
             or use TerrainQuality with FogTier::Low for MSAA-compatible inline fog."
        );
        return;
    }
    let Some(render_pipeline) = pipeline_cache.get_render_pipeline(pipeline_id.0) else {
        return;
    };
    let (Some(view_binding), Some(fog_binding)) = (
        view_uniforms.uniforms.binding(),
        fog_uniforms.uniforms().binding(),
    ) else {
        return;
    };

    let post = view_target.post_process_write();
    let bind_group = ctx.render_device().create_bind_group(
        Some("height_fog_bind_group"),
        &pipeline_cache.get_bind_group_layout(&pipeline.layout),
        &BindGroupEntries::sequential((
            view_binding,
            depth.view(),
            post.source,
            &pipeline.sampler,
            fog_binding,
        )),
    );

    let mut render_pass = ctx.begin_tracked_render_pass(RenderPassDescriptor {
        label: Some("height_fog"),
        color_attachments: &[Some(RenderPassColorAttachment {
            view: post.destination,
            depth_slice: None,
            resolve_target: None,
            ops: Operations {
                load: LoadOp::Clear(Default::default()),
                store: StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });

    render_pass.set_render_pipeline(render_pipeline);
    render_pass.set_bind_group(0, &bind_group, &[view_offset.offset]);
    render_pass.draw(0..3, 0..1);
}
