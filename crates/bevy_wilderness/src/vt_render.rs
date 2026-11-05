//! Render-world half of the texture clipmap (`vt.rs`): drains composite
//! requests within the per-frame texel budget and draws each into its level's
//! array layer, before any camera renders.

use std::collections::{HashMap, VecDeque};

use bevy::{
    asset::{load_embedded_asset, AssetServer},
    prelude::*,
    render::{
        render_asset::RenderAssets,
        render_resource::{
            binding_types::{sampler, texture_2d, texture_2d_array, uniform_buffer},
            BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries,
            CachedRenderPipelineId, ColorTargetState, ColorWrites, DynamicUniformBuffer,
            FragmentState, LoadOp, Operations, PipelineCache, RenderPassColorAttachment,
            RenderPassDescriptor, RenderPipelineDescriptor, SamplerBindingType, ShaderStages,
            ShaderType, StoreOp, TextureFormat, TextureSampleType, TextureViewDescriptor,
            TextureViewDimension, UniformBuffer, VertexState,
        },
        diagnostic::RecordDiagnostics,
        renderer::{RenderContext, RenderDevice, RenderGraph, RenderGraphSystems, RenderQueue},
        texture::GpuImage,
        ExtractSchedule, MainWorld, RenderApp, RenderStartup,
    },
};

use crate::vt::{VtDone, VtMessage, VtOutbox, VtRequest, VtStats};

/// The render world's queue of requests, filled at extraction.
#[derive(Resource, Default)]
struct VtInbox {
    queue: VecDeque<VtRequest>,
    budget: u32,
}

/// Where in framebuffer space one draw lands, and the world it maps to. Matches
/// `CompositeDraw` in `vt_composite.wesl`.
#[derive(Clone, Copy, Debug, Default, ShaderType)]
struct CompositeDraw {
    origin: Vec2,
    texel: f32,
    _pad: f32,
}

#[derive(Resource)]
struct VtPipeline {
    terrain_layout: BindGroupLayoutDescriptor,
    draw_layout: BindGroupLayoutDescriptor,
    id: CachedRenderPipelineId,
}

pub(crate) struct VtRenderPlugin;

impl Plugin for VtRenderPlugin {
    fn build(&self, app: &mut App) {
        let done = VtDone::default();
        let stats = VtStats::default();
        app.init_resource::<VtOutbox>()
            .insert_resource(done.clone())
            .insert_resource(stats.clone());
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .insert_resource(done)
            .insert_resource(stats)
            .init_resource::<VtInbox>()
            .add_systems(RenderStartup, init_vt_pipeline)
            .add_systems(ExtractSchedule, extract_vt_messages)
            .add_systems(RenderGraph, composite_vt.in_set(RenderGraphSystems::Begin));
    }
}

fn init_vt_pipeline(
    mut commands: Commands,
    pipeline_cache: Res<PipelineCache>,
    asset_server: Res<AssetServer>,
) {
    let terrain_layout = BindGroupLayoutDescriptor::new(
        "terrain_vt_composite_terrain",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::FRAGMENT,
            (
                texture_2d(TextureSampleType::Float { filterable: false }),
                texture_2d_array(TextureSampleType::Float { filterable: true }),
                sampler(SamplerBindingType::Filtering),
                texture_2d_array(TextureSampleType::Float { filterable: true }),
                sampler(SamplerBindingType::Filtering),
                texture_2d_array(TextureSampleType::Float { filterable: true }),
                sampler(SamplerBindingType::Filtering),
                uniform_buffer::<crate::vt::TerrainParams>(false),
                uniform_buffer::<crate::vt::CompositeTerrain>(false),
                texture_2d(TextureSampleType::Float { filterable: false }),
            ),
        ),
    );
    let draw_layout = BindGroupLayoutDescriptor::new(
        "terrain_vt_composite_draw",
        &BindGroupLayoutEntries::single(
            ShaderStages::FRAGMENT,
            uniform_buffer::<CompositeDraw>(true),
        ),
    );
    let shader = load_embedded_asset!(asset_server.as_ref(), "vt_composite.wesl");
    let target = |format| {
        Some(ColorTargetState {
            format,
            blend: None,
            write_mask: ColorWrites::ALL,
        })
    };
    let id = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
        label: Some("terrain_vt_composite".into()),
        layout: vec![terrain_layout.clone(), draw_layout.clone()],
        vertex: VertexState {
            shader: shader.clone(),
            entry_point: Some("vertex".into()),
            ..default()
        },
        fragment: Some(FragmentState {
            shader,
            entry_point: Some("fragment".into()),
            targets: vec![
                target(TextureFormat::Rgba8UnormSrgb),
                target(TextureFormat::Rgba8Unorm),
            ],
            ..default()
        }),
        ..default()
    });
    commands.insert_resource(VtPipeline {
        terrain_layout,
        draw_layout,
        id,
    });
}

/// One request's share of this frame, in one layer.
struct Batch {
    clipmap: Entity,
    layer: u32,
    /// Framebuffer rect (x, y, w, h) and the draw-uniform offset for each piece.
    pieces: Vec<([u32; 4], u32)>,
}

/// `rect` (level texel coordinates) split where it wraps the layer, as
/// `(world rect, slot origin)` pairs. Texel `k` lives in slot `k + size/2`
/// (mod `size`), matching the main pass's `world / coverage + 0.5`.
fn toroidal_pieces(rect: IRect, size: i32) -> Vec<(IRect, IVec2)> {
    let split = |min: i32, max: i32| -> Vec<(i32, i32, i32)> {
        let slot = (min + size / 2).rem_euclid(size);
        let len = max - min;
        if slot + len <= size {
            vec![(min, max, slot)]
        } else {
            let first = size - slot;
            vec![(min, min + first, slot), (min + first, max, 0)]
        }
    };
    let mut pieces = Vec::new();
    for (x0, x1, sx) in split(rect.min.x, rect.max.x) {
        for (y0, y1, sy) in split(rect.min.y, rect.max.y) {
            pieces.push((IRect::new(x0, y0, x1, y1), IVec2::new(sx, sy)));
        }
    }
    pieces
}

fn extract_vt_messages(mut main_world: ResMut<MainWorld>, mut inbox: ResMut<VtInbox>) {
    let mut outbox = main_world.resource_mut::<VtOutbox>();
    inbox.budget = outbox.budget;
    for message in outbox.messages.drain(..) {
        match message {
            VtMessage::Request(request) => inbox.queue.push_back(request),
            VtMessage::Cancel(clipmap) => inbox.queue.retain(|r| r.clipmap != clipmap),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn composite_vt(
    mut inbox: ResMut<VtInbox>,
    done_out: Res<VtDone>,
    stats: Res<VtStats>,
    pipeline: Option<Res<VtPipeline>>,
    pipeline_cache: Res<PipelineCache>,
    images: Res<RenderAssets<GpuImage>>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    mut draws: Local<DynamicUniformBuffer<CompositeDraw>>,
    mut ctx: RenderContext,
) {
    let budget = inbox.budget;
    let queue = &mut inbox.queue;
    if queue.is_empty() {
        return;
    }
    let Some(pipeline) = pipeline else {
        return;
    };
    let Some(render_pipeline) = pipeline_cache.get_render_pipeline(pipeline.id) else {
        return;
    };

    let mut remaining = budget as i64;
    let mut batches: Vec<Batch> = Vec::new();
    let mut terrain_groups: HashMap<Entity, BindGroup> = HashMap::new();
    let mut targets: HashMap<Entity, (AssetId<Image>, AssetId<Image>)> = HashMap::new();
    let mut done = Vec::new();
    draws.clear();

    while remaining > 0 {
        let Some(mut request) = queue.pop_front() else {
            break;
        };
        let sources = request.sources;
        let (Some(heightmap), Some(albedo), Some(normal), Some(orm), Some(vt_albedo), Some(_), Some(masks)) = (
            images.get(sources.heightmap),
            images.get(sources.albedo_array),
            images.get(sources.normal_array),
            images.get(sources.orm_array),
            images.get(sources.vt_albedo),
            images.get(sources.vt_normal),
            images.get(sources.masks),
        ) else {
            queue.push_front(request);
            break;
        };
        // The layout's images land a frame or more after its first requests.
        let extent = vt_albedo.texture_descriptor.size;
        if extent.width != request.size || extent.depth_or_array_layers <= request.layer {
            queue.push_front(request);
            break;
        }

        if !terrain_groups.contains_key(&request.clipmap) {
            let mut params = UniformBuffer::from(request.params);
            params.write_buffer(&render_device, &render_queue);
            let mut terrain = UniformBuffer::from(request.terrain);
            terrain.write_buffer(&render_device, &render_queue);
            let (Some(params), Some(terrain)) = (params.binding(), terrain.binding()) else {
                queue.push_front(request);
                break;
            };
            let group = render_device.create_bind_group(
                "terrain_vt_composite_terrain",
                &pipeline_cache.get_bind_group_layout(&pipeline.terrain_layout),
                &BindGroupEntries::sequential((
                    &heightmap.texture_view,
                    &albedo.texture_view,
                    &albedo.sampler,
                    &normal.texture_view,
                    &normal.sampler,
                    &orm.texture_view,
                    &orm.sampler,
                    params,
                    terrain,
                    &masks.texture_view,
                )),
            );
            terrain_groups.insert(request.clipmap, group);
            targets.insert(request.clipmap, (sources.vt_albedo, sources.vt_normal));
        }

        let width = request.rect.width().max(1);
        let rows = ((remaining / width as i64).max(1) as i32).min(request.rect.height());
        let part = IRect::new(
            request.rect.min.x,
            request.rect.min.y,
            request.rect.max.x,
            request.rect.min.y + rows,
        );
        remaining -= (rows * width) as i64;

        let mut pieces = Vec::new();
        for (world, slot) in toroidal_pieces(part, request.size as i32) {
            let offset = draws.push(&CompositeDraw {
                origin: (world.min - slot).as_vec2() * request.texel,
                texel: request.texel,
                _pad: 0.0,
            });
            pieces.push((
                [
                    slot.x as u32,
                    slot.y as u32,
                    world.width() as u32,
                    world.height() as u32,
                ],
                offset,
            ));
        }
        batches.push(Batch {
            clipmap: request.clipmap,
            layer: request.layer,
            pieces,
        });

        if part.max.y >= request.rect.max.y {
            done.push(request.id);
        } else {
            request.rect.min.y = part.max.y;
            queue.push_front(request);
        }
    }
    if batches.is_empty() {
        return;
    }

    draws.write_buffer(&render_device, &render_queue);
    let Some(draw_binding) = draws.binding() else {
        return;
    };
    let draw_group = render_device.create_bind_group(
        "terrain_vt_composite_draw",
        &pipeline_cache.get_bind_group_layout(&pipeline.draw_layout),
        &BindGroupEntries::single(draw_binding),
    );

    let diagnostics = ctx.diagnostic_recorder();
    let diagnostics = diagnostics.as_deref();
    let span = diagnostics.time_span(ctx.command_encoder(), crate::vt::VT_COMPOSITE_SPAN);
    for batch in &batches {
        let (albedo_id, normal_id) = targets[&batch.clipmap];
        let (Some(albedo), Some(normal)) = (images.get(albedo_id), images.get(normal_id)) else {
            continue;
        };
        let layer_view = |image: &GpuImage| {
            image.texture.create_view(&TextureViewDescriptor {
                label: Some("terrain_vt_layer"),
                dimension: Some(TextureViewDimension::D2),
                base_array_layer: batch.layer,
                array_layer_count: Some(1),
                ..default()
            })
        };
        let albedo_view = layer_view(albedo);
        let normal_view = layer_view(normal);
        let attachment = |view| {
            Some(RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Load,
                    store: StoreOp::Store,
                },
            })
        };
        let mut pass = ctx
            .command_encoder()
            .begin_render_pass(&RenderPassDescriptor {
                label: Some("terrain_vt_composite"),
                color_attachments: &[attachment(&albedo_view), attachment(&normal_view)],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        pass.set_pipeline(render_pipeline);
        pass.set_bind_group(0, &terrain_groups[&batch.clipmap], &[]);
        for ([x, y, w, h], offset) in &batch.pieces {
            pass.set_bind_group(1, &draw_group, &[*offset]);
            pass.set_viewport(*x as f32, *y as f32, *w as f32, *h as f32, 0.0, 1.0);
            pass.set_scissor_rect(*x, *y, *w, *h);
            pass.draw(0..3, 0..1);
        }
    }

    span.end(ctx.command_encoder());

    let drawn = (budget as i64 - remaining).max(0) as u64;
    let queued: u64 = queue
        .iter()
        .map(|r| (r.rect.width() * r.rect.height()) as u64)
        .sum();
    {
        let mut window = stats.0.lock().unwrap();
        window.frames += 1;
        window.texels += drawn;
        window.max_texels = window.max_texels.max(drawn);
        window.queued = queued;
    }
    done_out.0.lock().unwrap().extend(done);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_wrap_and_stay_in_the_layer() {
        let size = 16;
        // Slots 8 + (-12..-4) = -4..4, wrapping at 0.
        let pieces = toroidal_pieces(IRect::new(-12, 0, -4, 4), size);
        assert_eq!(pieces.len(), 2);
        let total: i32 = pieces.iter().map(|(r, _)| r.width() * r.height()).sum();
        assert_eq!(total, 32);
        for (rect, slot) in &pieces {
            assert!(slot.x >= 0 && slot.x + rect.width() <= size);
            assert!(slot.y >= 0 && slot.y + rect.height() <= size);
            assert_eq!((rect.min.x + size / 2).rem_euclid(size), slot.x);
        }
    }
}
