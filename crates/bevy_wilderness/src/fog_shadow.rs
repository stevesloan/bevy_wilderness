//! The fog shadow volume: where the terrain's shadow falls in the fog around
//! the viewer, so the fog takes the sun only where the terrain lets it through.
//!
//! A fragment can't afford to walk its own view ray through the sun shadow
//! field in steps short enough to find a ridge's shadow kilometres out, and too
//! few steps show as bands and false shafts. So the walk is done once a frame
//! for a grid of directions around the viewer, in hundreds of steps each, into
//! a 3D texture every fogged material reads with one filtered fetch
//! (`fog_sunlit_share` in `fog_functions.wesl`, which also has its layout).
//!
//! The volume is aligned to the world, not the camera: turning the view
//! changes nothing in it, both eyes of a headset read the same one, and it's
//! filled again only when the viewer moves or the fog, the field or its size
//! changes.
//!
//! Bind [`FogShadowVolume::image`] in a material of your own as a 3D texture
//! with its own sampler, and follow the handle: it's replaced when
//! [`HeightFog::shadow`](crate::HeightFog::shadow) changes size.

use bevy::{
    asset::{load_embedded_asset, AssetServer, RenderAssetUsages},
    image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor},
    prelude::*,
    render::{
        render_asset::RenderAssets,
        render_resource::{
            binding_types::{sampler, texture_2d, uniform_buffer},
            BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries,
            CachedRenderPipelineId, ColorTargetState, ColorWrites, DynamicUniformBuffer, Extent3d,
            FragmentState, LoadOp, Operations, PipelineCache, RenderPassColorAttachment,
            RenderPassDescriptor, RenderPipelineDescriptor, SamplerBindingType, ShaderStages,
            ShaderType, StoreOp, TextureDescriptor, TextureDimension, TextureFormat,
            TextureSampleType, TextureUsages, TextureView, VertexState,
        },
        renderer::{RenderContext, RenderDevice, RenderGraph, RenderGraphSystems, RenderQueue},
        texture::GpuImage,
        Extract, ExtractSchedule, RenderApp, RenderStartup,
    },
};

use crate::{HeightFogParams, SunShadowParams, TerrainFog, TerrainSunShadow};

const VOLUME_FORMAT: TextureFormat = TextureFormat::R16Float;
/// 32 bits: slices add to it one after another, and each one's share is small.
const CARRIED_FORMAT: TextureFormat = TextureFormat::Rg32Float;
/// Metres the viewer moves before the volume is filled again. Fog starts
/// metres from the eye, so this much is under a texel of it.
const REFILL_DISTANCE: f32 = 0.05;

/// The fog shadow volume's resolution.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Reflect)]
pub struct FogShadowSize {
    /// Directions around the horizon: a shaft's edge is about one wide.
    pub azimuth: u32,
    /// Directions from straight down to straight up, closest at the horizon.
    pub elevation: u32,
    /// Distances along each direction, closest at the viewer.
    pub slices: u32,
    /// Stretches each slice's length of ray is walked in: what finds a narrow
    /// shadow, at a fetch of the field each.
    pub steps: u32,
}

impl FogShadowSize {
    pub const MEDIUM: Self = Self {
        azimuth: 256,
        elevation: 64,
        slices: 32,
        steps: 3,
    };
    pub const HIGH: Self = Self {
        azimuth: 512,
        elevation: 96,
        slices: 48,
        steps: 4,
    };

    fn extent(self) -> UVec3 {
        UVec3::new(self.azimuth, self.elevation, self.slices)
            .clamp(UVec3::splat(2), UVec3::splat(1024))
    }
}

/// The entity the volume is centred on: the eye, or the midpoint of a
/// headset's two. Without one it follows the active 3D camera drawn last.
#[derive(Component, Default, Reflect, Debug)]
#[reflect(Component)]
pub struct FogShadowViewer;

/// The volume as materials bind it.
#[derive(Resource, Clone, Debug)]
pub struct FogShadowVolume {
    /// A 3D texture. Always bindable: with no
    /// [`HeightFog::shadow`](crate::HeightFog::shadow) it's a stand-in nothing
    /// reads.
    pub image: Handle<Image>,
    size: UVec3,
}

impl FromWorld for FogShadowVolume {
    fn from_world(world: &mut World) -> Self {
        let size = UVec3::splat(2);
        Self {
            image: world
                .resource_mut::<Assets<Image>>()
                .add(volume_image(size)),
            size,
        }
    }
}

/// No CPU data: a fill writes every texel, and until one lands the texture
/// reads as zero, nothing shaded.
fn volume_image(size: UVec3) -> Image {
    Image {
        data: None,
        texture_descriptor: TextureDescriptor {
            label: Some("fog_shadow_volume"),
            size: Extent3d {
                width: size.x,
                height: size.y,
                depth_or_array_layers: size.z,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D3,
            format: VOLUME_FORMAT,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        },
        // The azimuth wraps; the shader keeps the other two on texel centres.
        sampler: ImageSampler::Descriptor(ImageSamplerDescriptor {
            address_mode_u: ImageAddressMode::Repeat,
            address_mode_v: ImageAddressMode::ClampToEdge,
            address_mode_w: ImageAddressMode::ClampToEdge,
            mag_filter: ImageFilterMode::Linear,
            min_filter: ImageFilterMode::Linear,
            mipmap_filter: ImageFilterMode::Nearest,
            ..default()
        }),
        asset_usage: RenderAssetUsages::RENDER_WORLD,
        ..default()
    }
}

/// A new image for a new size, not the old one resized: a material follows the
/// handle, and would keep a resized texture's old bind group.
fn size_volume(
    fog: Res<TerrainFog>,
    mut volume: ResMut<FogShadowVolume>,
    mut images: ResMut<Assets<Image>>,
) {
    let size = fog.0.shadow.map_or(UVec3::splat(2), FogShadowSize::extent);
    if volume.size != size {
        volume.image = images.add(volume_image(size));
        volume.size = size;
    }
}

pub(crate) struct FogShadowPlugin;

impl Plugin for FogShadowPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<FogShadowViewer>()
            .init_resource::<FogShadowVolume>()
            .add_systems(Update, size_volume);
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .init_resource::<Wanted>()
            .add_systems(RenderStartup, init_pipeline)
            .add_systems(ExtractSchedule, extract_wanted)
            .add_systems(RenderGraph, fill_volume.in_set(RenderGraphSystems::Begin));
    }
}

/// Everything a fill reads: one is due when any of it differs from the last.
#[derive(Clone, PartialEq)]
struct Fill {
    volume: AssetId<Image>,
    field: AssetId<Image>,
    shadow: SunShadowParams,
    fog: HeightFogParams,
    center: Vec3,
    size: UVec3,
    steps: u32,
}

impl Fill {
    fn same_but_nearby(&self, last: &Self) -> bool {
        let moved = Self {
            center: last.center,
            ..self.clone()
        };
        moved == *last && self.center.distance(last.center) < REFILL_DISTANCE
    }
}

/// The volume the frame wants; `None` with nothing to shade the fog.
#[derive(Resource, Default)]
struct Wanted(Option<Fill>);

fn extract_wanted(
    mut wanted: ResMut<Wanted>,
    volume: Extract<Res<FogShadowVolume>>,
    fog: Extract<Res<TerrainFog>>,
    // Only the first clipmap's field: a material binds one volume.
    fields: Extract<Query<&TerrainSunShadow>>,
    viewers: Extract<Query<&GlobalTransform, With<FogShadowViewer>>>,
    cameras: Extract<Query<(&GlobalTransform, &Camera), With<Camera3d>>>,
) {
    let center = viewers.iter().next().or_else(|| {
        cameras
            .iter()
            .filter(|(_, camera)| camera.is_active)
            .max_by_key(|(_, camera)| camera.order)
            .map(|(transform, _)| transform)
    });
    wanted.0 = (|| {
        let size = fog.0.shadow?;
        let field = fields.iter().next()?;
        let foggy = fog.0.density + fog.0.haze_density > 0.0;
        (foggy && field.params.valid >= 0.5 && volume.size == size.extent()).then_some(Fill {
            volume: volume.image.id(),
            field: field.field.id(),
            shadow: field.params,
            fog: HeightFogParams::from(&fog.0),
            center: center?.translation(),
            size: volume.size,
            steps: size.steps.max(1),
        })
    })();
}

/// Matches `Bake` in `fog_shadow_volume.wesl`.
#[derive(Clone, ShaderType)]
struct BakeUniform {
    fog: HeightFogParams,
    shadow: SunShadowParams,
    center: Vec3,
    slice: u32,
    size: UVec3,
    steps: u32,
}

#[derive(Resource)]
struct VolumePipeline {
    scene_layout: BindGroupLayoutDescriptor,
    carried_layout: BindGroupLayoutDescriptor,
    id: CachedRenderPipelineId,
}

fn init_pipeline(
    mut commands: Commands,
    pipeline_cache: Res<PipelineCache>,
    asset_server: Res<AssetServer>,
) {
    let scene_layout = BindGroupLayoutDescriptor::new(
        "fog_shadow_volume_scene",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::FRAGMENT,
            (
                uniform_buffer::<BakeUniform>(true),
                texture_2d(TextureSampleType::Float { filterable: true }),
                sampler(SamplerBindingType::Filtering),
            ),
        ),
    );
    let carried_layout = BindGroupLayoutDescriptor::new(
        "fog_shadow_volume_carried",
        &BindGroupLayoutEntries::single(
            ShaderStages::FRAGMENT,
            texture_2d(TextureSampleType::Float { filterable: false }),
        ),
    );
    let shader = load_embedded_asset!(asset_server.as_ref(), "fog_shadow_volume.wesl");
    let target = |format| {
        Some(ColorTargetState {
            format,
            blend: None,
            write_mask: ColorWrites::ALL,
        })
    };
    let id = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
        label: Some("fog_shadow_volume".into()),
        layout: vec![scene_layout.clone(), carried_layout.clone()],
        vertex: VertexState {
            shader: shader.clone(),
            entry_point: Some("vertex".into()),
            ..default()
        },
        fragment: Some(FragmentState {
            shader,
            entry_point: Some("fragment".into()),
            targets: vec![target(VOLUME_FORMAT), target(CARRIED_FORMAT)],
            ..default()
        }),
        ..default()
    });
    commands.insert_resource(VolumePipeline {
        scene_layout,
        carried_layout,
        id,
    });
}

/// What the last fill left: what it read, and the two textures slices carry
/// their running sum in, each pass reading the one the pass before wrote.
#[derive(Default)]
struct Filled {
    last: Option<Fill>,
    carried: Option<(UVec2, [TextureView; 2])>,
}

#[allow(clippy::too_many_arguments)]
fn fill_volume(
    wanted: Res<Wanted>,
    pipeline: Option<Res<VolumePipeline>>,
    pipeline_cache: Res<PipelineCache>,
    images: Res<RenderAssets<GpuImage>>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    mut slices: Local<DynamicUniformBuffer<BakeUniform>>,
    mut filled: Local<Filled>,
    mut ctx: RenderContext,
) {
    let Some(fill) = &wanted.0 else {
        filled.last = None;
        return;
    };
    if filled
        .last
        .as_ref()
        .is_some_and(|last| fill.same_but_nearby(last))
    {
        return;
    }
    let (Some(pipeline), Some(volume), Some(field)) =
        (pipeline, images.get(fill.volume), images.get(fill.field))
    else {
        return;
    };
    let Some(render_pipeline) = pipeline_cache.get_render_pipeline(pipeline.id) else {
        return;
    };
    let extent = volume.texture_descriptor.size;
    if UVec3::new(extent.width, extent.height, extent.depth_or_array_layers) != fill.size {
        return;
    }

    let face = fill.size.truncate();
    if filled
        .carried
        .as_ref()
        .is_none_or(|(size, _)| *size != face)
    {
        let view = || {
            render_device
                .create_texture(&TextureDescriptor {
                    label: Some("fog_shadow_volume_carried"),
                    size: Extent3d {
                        width: face.x,
                        height: face.y,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: TextureDimension::D2,
                    format: CARRIED_FORMAT,
                    usage: TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&default())
        };
        filled.carried = Some((face, [view(), view()]));
    }
    let Some((_, carried)) = &filled.carried else {
        return;
    };

    slices.clear();
    let offsets: Vec<u32> = (1..fill.size.z)
        .map(|slice| {
            slices.push(&BakeUniform {
                fog: fill.fog.clone(),
                shadow: fill.shadow,
                center: fill.center,
                slice,
                size: fill.size,
                steps: fill.steps,
            })
        })
        .collect();
    slices.write_buffer(&render_device, &render_queue);
    let Some(slice_binding) = slices.binding() else {
        return;
    };
    let scene_group = render_device.create_bind_group(
        "fog_shadow_volume_scene",
        &pipeline_cache.get_bind_group_layout(&pipeline.scene_layout),
        &BindGroupEntries::sequential((slice_binding, &field.texture_view, &field.sampler)),
    );
    let carried_layout = pipeline_cache.get_bind_group_layout(&pipeline.carried_layout);
    let carried_groups = [0, 1].map(|index| {
        render_device.create_bind_group(
            "fog_shadow_volume_carried",
            &carried_layout,
            &BindGroupEntries::single(&carried[index]),
        )
    });

    let encoder = ctx.command_encoder();
    encoder.push_debug_group("fog_shadow_volume");
    let attachment = |view, depth_slice, load| {
        Some(RenderPassColorAttachment {
            view,
            depth_slice,
            resolve_target: None,
            ops: Operations {
                load,
                store: StoreOp::Store,
            },
        })
    };
    // The first slice is the fog's start: nothing shaded yet.
    drop(encoder.begin_render_pass(&RenderPassDescriptor {
        label: Some("fog_shadow_volume_start"),
        color_attachments: &[attachment(
            &volume.texture_view,
            Some(0),
            LoadOp::Clear(Default::default()),
        )],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    }));
    for (slice, offset) in (1..fill.size.z).zip(&offsets) {
        let write = (slice % 2) as usize;
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("fog_shadow_volume_slice"),
            color_attachments: &[
                attachment(&volume.texture_view, Some(slice), LoadOp::Load),
                attachment(&carried[write], None, LoadOp::Load),
            ],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(render_pipeline);
        pass.set_bind_group(0, &scene_group, &[*offset]);
        pass.set_bind_group(1, &carried_groups[1 - write], &[]);
        pass.draw(0..3, 0..1);
    }
    encoder.pop_debug_group();
    filled.last = Some(fill.clone());
}
