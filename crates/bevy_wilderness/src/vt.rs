//! The terrain's shading cache as a texture clipmap: one array layer per level,
//! each a fixed number of texels square. Fine levels follow the clipmap's target
//! and halve in density outward; the last level covers the whole world. Each
//! level is addressed toroidally (`world / coverage + 0.5`, wrapped by the
//! sampler), so when the target moves only the strips entering a level's
//! window are composited, by `vt_render`.
//!
//! A level's *visible* rect is what the main pass may read. When a level moves,
//! the texels about to be overwritten are the ones leaving its window, so the
//! visible rect shrinks to the overlap of the old and new windows in the same
//! frame the strips are queued, and grows to the new window once the render
//! world acknowledges them. The main pass falls back to coarser levels outside a
//! level's visible rect.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use bevy::{
    asset::RenderAssetUsages,
    image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor},
    pbr::ExtendedMaterial,
    prelude::*,
    render::render_resource::{
        Extent3d, ShaderType, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
        TextureViewDescriptor, TextureViewDimension,
    },
};

use crate::{
    Clipmap, ClipmapReady, MAX_TERRAIN_LAYERS, TerrainQuality, clipmap::ClipmapMaterials,
    material::GridMaterial, rvt::ClipmapRvt,
};

/// Levels the main pass's uniform has room for, the whole-world level included.
pub(crate) const MAX_VT_LEVELS: usize = 8;

/// One level as the main pass reads it. Matches `VtLevel` in `terrain.wesl`.
#[derive(Clone, Copy, Debug, Default, PartialEq, ShaderType, Reflect)]
pub(crate) struct VtLevelGpu {
    /// Visible world rect: min x, min z, max x, max z. Empty when min > max.
    rect: Vec4,
    /// Texel size (m), coverage (m), fade band at the rect's edge (m), unused.
    info: Vec4,
}

/// The clipmap as the main pass reads it. Matches `VtParams` in `terrain.wesl`.
#[derive(Clone, Copy, Debug, PartialEq, ShaderType, Reflect)]
pub(crate) struct VtParams {
    levels: [VtLevelGpu; MAX_VT_LEVELS],
    /// Levels in use; the last is the whole-world level.
    count: u32,
    /// Texels per level side.
    size: u32,
}

impl Default for VtParams {
    fn default() -> Self {
        Self {
            levels: [VtLevelGpu::default(); MAX_VT_LEVELS],
            count: 0,
            size: 1,
        }
    }
}

/// Per-layer splat parameters packed for the GPU. `Vec4` lanes index the layers.
/// Matches `TerrainParams` in `vt_composite.wesl`.
#[derive(Clone, Copy, Debug, Default, ShaderType)]
pub(crate) struct TerrainParams {
    tiling_scale: Vec4,
    height_blend: Vec4,
    roughness: Vec4,
    normal_strength: Vec4,
    /// Slope band per layer (radians), ramping over `slope_blend` at each edge.
    /// No rule = a band so wide its ramps never fire.
    slope_min: Vec4,
    slope_max: Vec4,
    slope_blend: Vec4,
    /// World-height band per layer (metres), same shape as the slope band.
    height_min: Vec4,
    height_max: Vec4,
    height_range_blend: Vec4,
    /// Curvature band per layer (cavity scale, 0.5 = flat).
    cavity_min: Vec4,
    cavity_max: Vec4,
    cavity_blend: Vec4,
    /// Mask channel per layer; negative = no mask rule.
    mask_channel: Vec4,
    mask_min: Vec4,
    mask_max: Vec4,
    mask_blend: Vec4,
    /// Per mask channel: albedo multiplier (rgb) and strength (w).
    mask_tint: [Vec4; 4],
    /// Per mask channel: the ramp's start (x) and end (y).
    mask_tint_range: [Vec4; 4],
    layer_count: u32,
}

impl TerrainParams {
    pub(crate) fn from_clipmap(clipmap: &Clipmap) -> Self {
        let mut tiling_scale = [1.0f32; MAX_TERRAIN_LAYERS];
        let mut height_blend = [0.0f32; MAX_TERRAIN_LAYERS];
        let mut roughness = [1.0f32; MAX_TERRAIN_LAYERS];
        let mut normal_strength = [1.0f32; MAX_TERRAIN_LAYERS];
        let mut slope_min = [-10.0f32; MAX_TERRAIN_LAYERS];
        let mut slope_max = [10.0f32; MAX_TERRAIN_LAYERS];
        let mut slope_blend = [0.01f32; MAX_TERRAIN_LAYERS];
        let mut height_min = [-1.0e9f32; MAX_TERRAIN_LAYERS];
        let mut height_max = [1.0e9f32; MAX_TERRAIN_LAYERS];
        let mut height_range_blend = [1.0f32; MAX_TERRAIN_LAYERS];
        let mut cavity_min = [-10.0f32; MAX_TERRAIN_LAYERS];
        let mut cavity_max = [10.0f32; MAX_TERRAIN_LAYERS];
        let mut cavity_blend = [0.01f32; MAX_TERRAIN_LAYERS];
        let mut mask_channel = [-1.0f32; MAX_TERRAIN_LAYERS];
        let mut mask_min = [0.0f32; MAX_TERRAIN_LAYERS];
        let mut mask_max = [1.0f32; MAX_TERRAIN_LAYERS];
        let mut mask_blend = [0.01f32; MAX_TERRAIN_LAYERS];
        let tints = clipmap.masks.as_ref().map_or([crate::MaskTint::default(); 4], |m| m.tints);
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
            if let Some(c) = &layer.cavity {
                cavity_min[i] = c.min;
                cavity_max[i] = c.max;
                cavity_blend[i] = c.blend.max(1e-3);
            }
            if let Some(m) = &layer.mask {
                mask_channel[i] = f32::from(m.channel.min(3));
                mask_min[i] = m.min;
                mask_max[i] = m.max;
                mask_blend[i] = m.blend.max(1e-3);
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
            cavity_min: Vec4::from_array(cavity_min),
            cavity_max: Vec4::from_array(cavity_max),
            cavity_blend: Vec4::from_array(cavity_blend),
            mask_channel: Vec4::from_array(mask_channel),
            mask_min: Vec4::from_array(mask_min),
            mask_max: Vec4::from_array(mask_max),
            mask_blend: Vec4::from_array(mask_blend),
            mask_tint: tints.map(|t| t.color.extend(t.strength)),
            mask_tint_range: tints.map(|t| Vec4::new(t.from, t.to.max(t.from + 1e-3), 0.0, 0.0)),
            layer_count: clipmap.layers.len().min(MAX_TERRAIN_LAYERS) as u32,
        }
    }
}

/// Heightfield scalars the composite needs. Matches `CompositeTerrain` in
/// `vt_composite.wesl`.
#[derive(Clone, Copy, Debug, Default, ShaderType)]
pub(crate) struct CompositeTerrain {
    pub(crate) minmax: Vec2,
    pub(crate) texel_size: f32,
    pub(crate) looping: u32,
    /// 0 = the mask binding holds a stand-in; mask reads are 0.
    pub(crate) has_masks: u32,
}

/// The images one composite reads and writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct VtSources {
    pub(crate) heightmap: AssetId<Image>,
    pub(crate) albedo_array: AssetId<Image>,
    pub(crate) normal_array: AssetId<Image>,
    pub(crate) orm_array: AssetId<Image>,
    /// The clipmap's masks, or the heightmap standing in when it has none (the
    /// binding can't be empty; no layer reads it then).
    pub(crate) masks: AssetId<Image>,
    pub(crate) vt_albedo: AssetId<Image>,
    pub(crate) vt_normal: AssetId<Image>,
}

/// One rect of one level to composite.
#[derive(Clone, Debug)]
pub(crate) struct VtRequest {
    pub(crate) id: u64,
    /// The clipmap entity: requests sharing it share sources and parameters.
    pub(crate) clipmap: Entity,
    pub(crate) sources: VtSources,
    pub(crate) terrain: CompositeTerrain,
    pub(crate) params: TerrainParams,
    pub(crate) layer: u32,
    pub(crate) size: u32,
    pub(crate) texel: f32,
    /// Level texel coordinates; texel `k` spans `k·texel ..(k+1)·texel` metres.
    pub(crate) rect: IRect,
}

pub(crate) enum VtMessage {
    Request(VtRequest),
    /// Drop the clipmap's queued requests: its layout was rebuilt, or a refresh
    /// supersedes them.
    Cancel(Entity),
}

/// Messages for the render world, moved there at extraction. They must not
/// reach it any sooner: under pipelined rendering the render world draws the
/// previous frame while this one runs, with that frame's (wider) visible rects,
/// and would show texels a request overwrites.
#[derive(Resource, Default)]
pub(crate) struct VtOutbox {
    pub(crate) messages: Vec<VtMessage>,
    /// Texels composited per frame, from [`TerrainQuality::vt_texels_per_frame`].
    pub(crate) budget: u32,
}

/// Requests the render world has fully drawn, back to the main world. Shared
/// rather than extracted: a late acknowledgement only widens a visible rect a
/// frame later.
#[derive(Resource, Clone, Default)]
pub(crate) struct VtDone(pub(crate) Arc<Mutex<Vec<u64>>>);

/// Composite work since the last report, written by the render world.
#[derive(Default)]
pub(crate) struct VtStatsWindow {
    /// Frames that composited anything.
    pub(crate) frames: u32,
    pub(crate) texels: u64,
    pub(crate) max_texels: u64,
    /// Texels still queued at the last compositing frame.
    pub(crate) queued: u64,
}

#[derive(Resource, Clone, Default)]
pub(crate) struct VtStats(pub(crate) Arc<Mutex<VtStatsWindow>>);

/// The render diagnostic span around the composite passes.
pub(crate) const VT_COMPOSITE_SPAN: &str = "terrain_vt_composite";

/// Logs composite cost once a second while the clipmap is compositing: texels,
/// and GPU time per compositing frame when the app has
/// `RenderDiagnosticsPlugin` and the device has timestamp queries.
#[cfg(feature = "dev-controls")]
pub(crate) fn log_vt_stats(
    stats: Res<VtStats>,
    store: Option<Res<bevy::diagnostic::DiagnosticsStore>>,
    mut last: Local<Option<std::time::Instant>>,
) {
    let now = std::time::Instant::now();
    let since = *last.get_or_insert(now);
    if now.duration_since(since).as_secs_f32() < 1.0 {
        return;
    }
    *last = Some(now);
    let window = std::mem::take(&mut *stats.0.lock().unwrap());
    if window.frames == 0 {
        return;
    }
    let path = bevy::diagnostic::DiagnosticPath::new(format!(
        "render/{VT_COMPOSITE_SPAN}/elapsed_gpu"
    ));
    let gpu: Vec<f64> = store
        .as_ref()
        .and_then(|store| store.get(&path))
        .map(|diagnostic| {
            diagnostic
                .measurements()
                .filter(|m| m.time >= since)
                .map(|m| m.value)
                .collect()
        })
        .unwrap_or_default();
    let gpu = if gpu.is_empty() {
        "GPU time unavailable".to_string()
    } else {
        let max = gpu.iter().cloned().fold(0.0, f64::max);
        let avg = gpu.iter().sum::<f64>() / gpu.len() as f64;
        format!("GPU {avg:.2} ms avg, {max:.2} ms max")
    };
    info!(
        "terrain clipmap: {} compositing frames, {}k texels ({}k max/frame), {}k queued; {gpu}",
        window.frames,
        window.texels / 1000,
        window.max_texels / 1000,
        window.queued / 1000,
    );
}

#[derive(Debug)]
struct Job {
    window: IRect,
    outstanding: HashSet<u64>,
    /// Part of a refresh, which [`ClipmapReady`] waits for.
    refresh: bool,
}

#[derive(Debug)]
struct Level {
    texel: f32,
    /// The whole-world level: fixed at the world's centre.
    base: bool,
    /// The window whose texels are all composited.
    committed: Option<IRect>,
    /// What the main pass may read: the committed window, narrowed while a job
    /// overwrites part of it, and clipped to the world on finite terrain.
    visible: Option<IRect>,
    job: Option<Job>,
    /// Recomposite the whole window, keeping the old texels visible until the
    /// new ones land.
    force: bool,
}

#[derive(Debug)]
struct Layout {
    size: u32,
    world_size: f32,
    looping: bool,
    /// Finest first; the last is the whole-world level.
    levels: Vec<Level>,
}

/// The clipmap's shading cache. Images are placeholders until the heightmap
/// loads and the layout is known, then replaced in place, so material bindings
/// stay valid.
#[derive(Component)]
pub(crate) struct ClipmapVt {
    pub(crate) albedo: Handle<Image>,
    pub(crate) normal: Handle<Image>,
    layout: Option<Layout>,
    /// Set by a rebake: recomposite every level, old texels on screen meanwhile.
    pub(crate) refresh: bool,
    /// A refresh is in flight; [`ClipmapReady`] waits for it.
    refreshing: bool,
    /// The quality the layout was built with.
    built_with: Option<(u32, f32)>,
    /// Where the windows were last centred. Held while the target is missing
    /// (despawned, or not spawned yet), as the geometry holds, so a vanished
    /// target doesn't recomposite every level around the world origin.
    last_target: Option<Vec2>,
}

impl ClipmapVt {
    pub(crate) fn new(images: &mut Assets<Image>) -> Self {
        Self {
            albedo: images.add(vt_array_image(1, 1, TextureFormat::Rgba8UnormSrgb)),
            normal: images.add(vt_array_image(1, 2, TextureFormat::Rgba8Unorm)),
            layout: None,
            refresh: false,
            refreshing: false,
            built_with: None,
            last_target: None,
        }
    }

    /// Levels composited, out of the levels in the layout (0 of 0 before it's built).
    pub(crate) fn committed_levels(&self) -> (usize, usize) {
        self.layout.as_ref().map_or((0, 0), |layout| {
            let done = layout.levels.iter().filter(|l| l.committed.is_some()).count();
            (done, layout.levels.len())
        })
    }

    /// Every level is composited and no refresh is pending.
    pub(crate) fn ready(&self) -> bool {
        !self.refresh
            && !self.refreshing
            && self
                .layout
                .as_ref()
                .is_some_and(|layout| layout.levels.iter().all(|level| level.committed.is_some()))
    }
}

/// A render-target array of `layers` square layers. No CPU data: the composite
/// writes every texel the main pass reads.
fn vt_array_image(size: u32, layers: u32, format: TextureFormat) -> Image {
    Image {
        data: None,
        texture_descriptor: TextureDescriptor {
            label: Some("terrain_vt"),
            size: Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        },
        // A one-layer array still has to bind as an array.
        texture_view_descriptor: Some(TextureViewDescriptor {
            dimension: Some(TextureViewDimension::D2Array),
            ..default()
        }),
        // Repeat is the toroidal addressing; no mips, the levels are the mips.
        sampler: ImageSampler::Descriptor(ImageSamplerDescriptor {
            address_mode_u: ImageAddressMode::Repeat,
            address_mode_v: ImageAddressMode::Repeat,
            mag_filter: ImageFilterMode::Linear,
            min_filter: ImageFilterMode::Linear,
            mipmap_filter: ImageFilterMode::Nearest,
            ..default()
        }),
        asset_usage: RenderAssetUsages::RENDER_WORLD,
        ..default()
    }
}

fn build_layout(quality: &TerrainQuality, world_size: f32, looping: bool) -> Layout {
    let size = quality.vt_size.max(64) & !1;
    let mut levels = Vec::new();
    let mut texel = quality.vt_finest_texel.max(1e-3);
    while levels.len() + 1 < MAX_VT_LEVELS && texel * (size as f32) < world_size {
        levels.push(Level::new(texel, false));
        texel *= 2.0;
    }
    levels.push(Level::new(world_size / size as f32, true));
    Layout {
        size,
        world_size,
        looping,
        levels,
    }
}

impl Level {
    fn new(texel: f32, base: bool) -> Self {
        Self {
            texel,
            base,
            committed: None,
            visible: None,
            job: None,
            force: false,
        }
    }

    /// The level's window: centred on the world for the base level, otherwise
    /// on `target`, snapped to an eighth of the window so it moves in strips.
    fn window(&self, size: u32, target: Vec2) -> IRect {
        let half = size as i32 / 2;
        if self.base {
            return IRect::new(-half, -half, half, half);
        }
        let step = (size as i32 / 8).max(1);
        let center = ((target / (self.texel * step as f32)).round().as_ivec2()) * step;
        IRect::from_corners(center - half, center + half)
    }

    /// The world in this level's texel coordinates.
    fn world(&self, world_size: f32) -> IRect {
        let half = (world_size * 0.5 / self.texel).ceil() as i32;
        IRect::new(-half, -half, half, half)
    }
}

/// `a` minus `b`, as up to four disjoint rects.
fn subtract(a: IRect, b: IRect) -> Vec<IRect> {
    let b = a.intersect(b);
    if b.is_empty() {
        return vec![a];
    }
    [
        IRect::new(a.min.x, a.min.y, a.max.x, b.min.y),
        IRect::new(a.min.x, b.max.y, a.max.x, a.max.y),
        IRect::new(a.min.x, b.min.y, b.min.x, b.max.y),
        IRect::new(b.max.x, b.min.y, a.max.x, b.max.y),
    ]
    .into_iter()
    .filter(|r| !r.is_empty())
    .collect()
}

fn visible(rect: IRect, world: Option<IRect>) -> Option<IRect> {
    let rect = world.map_or(rect, |world| rect.intersect(world));
    (!rect.is_empty()).then_some(rect)
}

/// Moves each level's window with the clipmap's target, queues the strips that
/// enter it, commits acknowledged jobs, and publishes the visible rects.
#[allow(clippy::type_complexity)]
pub(crate) fn update_vt(
    quality: Res<TerrainQuality>,
    mut outbox: ResMut<VtOutbox>,
    done: Res<VtDone>,
    mut next_id: Local<u64>,
    mut images: ResMut<Assets<Image>>,
    transforms: Query<&GlobalTransform>,
    mut clipmaps: Query<(Entity, &Clipmap, &mut ClipmapVt, &ClipmapMaterials)>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
) {
    outbox.budget = quality.vt_texels_per_frame.max(1);
    let done: HashSet<u64> = done.0.lock().unwrap().drain(..).collect();
    let mut messages = Vec::new();

    for (entity, clipmap, mut vt, mats) in &mut clipmaps {
        let Some(heightmap) = images.get(&clipmap.heightmap) else {
            continue;
        };
        let world_size = clipmap.texel_size * heightmap.width() as f32;
        let quality_key = (quality.vt_size, quality.vt_finest_texel);
        if vt.layout.is_none() || vt.built_with != Some(quality_key) {
            messages.push(VtMessage::Cancel(entity));
            let layout = build_layout(&quality, world_size, clipmap.looping);
            let layers = layout.levels.len() as u32;
            // The normal array's second half holds each level's layer weights.
            for (handle, layers, format) in [
                (vt.albedo.clone(), layers, TextureFormat::Rgba8UnormSrgb),
                (vt.normal.clone(), layers * 2, TextureFormat::Rgba8Unorm),
            ] {
                if let Some(mut image) = images.get_mut(&handle) {
                    *image = vt_array_image(layout.size, layers, format);
                }
            }
            vt.layout = Some(layout);
            vt.built_with = Some(quality_key);
        }
        let refresh = std::mem::take(&mut vt.refresh);
        if refresh {
            messages.push(VtMessage::Cancel(entity));
        }
        let target = transforms
            .get(clipmap.target)
            .map(|t| t.translation().xz())
            .ok()
            .or(vt.last_target)
            .unwrap_or(Vec2::ZERO);
        vt.last_target = Some(target);
        let sources = VtSources {
            heightmap: clipmap.heightmap.id(),
            albedo_array: clipmap.albedo_array.id(),
            normal_array: clipmap.normal_array.id(),
            orm_array: clipmap.orm_array.id(),
            masks: clipmap.masks.as_ref().map_or(clipmap.heightmap.id(), |m| m.image.id()),
            vt_albedo: vt.albedo.id(),
            vt_normal: vt.normal.id(),
        };
        let terrain = CompositeTerrain {
            minmax: Vec2::new(clipmap.min, clipmap.max),
            texel_size: clipmap.texel_size,
            looping: clipmap.looping as u32,
            has_masks: clipmap.masks.is_some() as u32,
        };
        let params = TerrainParams::from_clipmap(clipmap);

        let vt = &mut *vt;
        let layout = vt.layout.as_mut().expect("built above");
        let size = layout.size;
        let finite = !layout.looping;
        let world_size = layout.world_size;
        let mut refreshing = false;
        // Coarse first: the render world drains requests in order, so new
        // ground gets a usable surface before a sharp one.
        for (layer, level) in layout.levels.iter_mut().enumerate().rev() {
            if refresh {
                level.job = None;
                level.force = true;
            }
            let world = finite.then(|| level.world(world_size));
            if let Some(job) = &mut level.job {
                job.outstanding.retain(|id| !done.contains(id));
                if job.outstanding.is_empty() {
                    level.committed = Some(job.window);
                    level.visible = visible(job.window, world);
                    level.job = None;
                }
            }
            if let Some(job) = &level.job {
                refreshing |= job.refresh;
                continue;
            }
            let window = level.window(size, target);
            if !level.force && level.committed == Some(window) {
                continue;
            }
            let pending = match level.committed {
                Some(committed) if !level.force => subtract(window, committed),
                _ => vec![window],
            };
            if !level.force {
                level.visible = level
                    .committed
                    .and_then(|committed| visible(committed.intersect(window), world));
            } else {
                level.visible = level
                    .visible
                    .and_then(|v| visible(v.intersect(window), world));
            }
            let mut outstanding = HashSet::new();
            for rect in pending {
                let Some(rect) = visible(rect, world) else {
                    continue;
                };
                *next_id += 1;
                outstanding.insert(*next_id);
                messages.push(VtMessage::Request(VtRequest {
                    id: *next_id,
                    clipmap: entity,
                    sources,
                    terrain,
                    params,
                    layer: layer as u32,
                    size,
                    texel: level.texel,
                    rect,
                }));
            }
            refreshing |= level.force && !outstanding.is_empty();
            if outstanding.is_empty() {
                level.committed = Some(window);
                level.visible = visible(window, world);
            } else {
                level.job = Some(Job {
                    window,
                    outstanding,
                    refresh: level.force,
                });
            }
            level.force = false;
        }
        vt.refreshing = refreshing;

        let gpu = vt_params(layout);
        for handle in [&mats.solid, &mats.wireframe] {
            let stale = materials
                .get(handle)
                .is_some_and(|material| material.extension.dev.vt != gpu);
            if stale && let Some(mut material) = materials.get_mut(handle) {
                material.extension.dev.vt = gpu;
            }
        }
    }

    outbox.messages.extend(messages);
}

fn vt_params(layout: &Layout) -> VtParams {
    let mut params = VtParams {
        count: layout.levels.len() as u32,
        size: layout.size,
        ..default()
    };
    let size = layout.size as f32;
    for (gpu, level) in params.levels.iter_mut().zip(&layout.levels) {
        gpu.rect = match level.visible {
            Some(rect) => Vec4::new(
                rect.min.x as f32 * level.texel,
                rect.min.y as f32 * level.texel,
                rect.max.x as f32 * level.texel,
                rect.max.y as f32 * level.texel,
            ),
            None => Vec4::new(1.0, 1.0, -1.0, -1.0),
        };
        gpu.info = Vec4::new(level.texel, size * level.texel, size / 8.0 * level.texel, 0.0);
    }
    params
}

/// Drops a removed clipmap's queued requests. They name its images, which go
/// with it, so the render world could never draw them.
pub(crate) fn cancel_removed_clipmap(removed: On<Remove<ClipmapVt>>, mut outbox: ResMut<VtOutbox>) {
    outbox.messages.push(VtMessage::Cancel(removed.entity));
}

/// [`ClipmapReady`] once the shadow field has baked and every level is
/// composited.
pub(crate) fn mark_clipmap_ready(
    mut commands: Commands,
    mut clipmaps: Query<(Entity, &mut ClipmapRvt, &ClipmapVt), Without<ClipmapReady>>,
) {
    for (entity, mut rvt, vt) in &mut clipmaps {
        if rvt.shadow_done && vt.ready() {
            rvt.ever_baked = true;
            commands.entity(entity).try_insert(ClipmapReady);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(rects: &[IRect]) -> i32 {
        rects.iter().map(|r| r.width() * r.height()).sum()
    }

    #[test]
    fn subtract_covers_the_difference_exactly() {
        let a = IRect::new(0, 0, 8, 8);
        let b = IRect::new(2, -4, 10, 6);
        let parts = subtract(a, b);
        assert_eq!(area(&parts), 64 - 6 * 6);
        for (i, p) in parts.iter().enumerate() {
            assert!(p.intersect(b).is_empty());
            for q in &parts[i + 1..] {
                assert!(p.intersect(*q).is_empty());
            }
        }
        assert_eq!(subtract(a, IRect::new(20, 20, 30, 30)), vec![a]);
        assert!(subtract(a, a).is_empty());
    }

    #[test]
    fn layout_halves_to_a_whole_world_level() {
        let quality = TerrainQuality {
            vt_size: 1024,
            vt_finest_texel: 0.5,
            ..default()
        };
        let layout = build_layout(&quality, 8192.0, false);
        let texels: Vec<f32> = layout.levels.iter().map(|l| l.texel).collect();
        assert_eq!(texels, vec![0.5, 1.0, 2.0, 4.0, 8.0]);
        assert!(layout.levels.last().unwrap().base);
    }

    #[test]
    fn despawning_a_clipmap_cancels_its_requests() {
        let mut world = World::new();
        world.init_resource::<VtOutbox>();
        world.add_observer(cancel_removed_clipmap);
        let mut images = Assets::<Image>::default();
        let clipmap = world.spawn(ClipmapVt::new(&mut images)).id();
        world.despawn(clipmap);
        let outbox = world.resource::<VtOutbox>();
        assert!(matches!(
            outbox.messages.as_slice(),
            [VtMessage::Cancel(entity)] if *entity == clipmap
        ));
    }

    #[test]
    fn windows_move_in_eighths() {
        let level = Level::new(1.0, false);
        let a = level.window(1024, Vec2::new(10.0, -10.0));
        assert_eq!(a, IRect::new(-512, -512, 512, 512));
        let b = level.window(1024, Vec2::new(70.0, 0.0));
        assert_eq!(b.min.x - a.min.x, 128);
    }
}
