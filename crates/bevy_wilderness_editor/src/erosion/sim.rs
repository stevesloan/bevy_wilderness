//! The wgpu driver for the pipe-model erosion kernels in `erosion.wgsl`:
//! owns the buffers and pipelines of one run, submits a bounded chunk of
//! iterations per call (so the app never stalls on a long simulation), and
//! reads the result back through a mapped staging buffer.
//!
//! This is deliberately *main-world* wgpu: pipelines come straight from
//! [`RenderDevice`] (which bevy clones into the main world) and chunks are
//! submitted on the shared [`RenderQueue`] — legal wgpu, and it sidesteps the
//! render graph entirely, which keeps the whole feature inside this crate and
//! testable against a headless device (see the tests in `erosion/mod.rs`).
//! `PipelineCache` is render-world-only, so the module is compiled here.
//!
//! Everything is deterministic *per device*: fixed iteration count, fixed
//! dispatch order, single-writer kernels. Cross-GPU bitwise identity is not
//! guaranteed (FMA contraction differs) — irrelevant for the editor, which
//! stores results rather than replaying them.

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use bevy::{
    math::UVec2,
    render::{
        render_resource::{
            BindGroup, BindGroupEntry, BindGroupLayout, BindGroupLayoutEntry, BindingType,
            Buffer, BufferBindingType, BufferDescriptor, BufferUsages, CommandEncoderDescriptor,
            ComputePassDescriptor, ComputePipeline, MapMode, PipelineCompilationOptions,
            PipelineLayoutDescriptor, PollType, RawComputePipelineDescriptor,
            ShaderModuleDescriptor, ShaderSource, ShaderStages,
        },
        renderer::{RenderDevice, RenderQueue},
    },
};

use crate::settings::ErosionSettings;

/// Kernel workgroup edge — keep in sync with `@workgroup_size` in the shader.
const WORKGROUP: u32 = 8;

/// Texel·iterations submitted per [`GpuErosion::step_chunk`] call (each
/// iteration is 5 dispatches over the domain). Bounds GPU time per frame so a
/// run degrades the frame rate instead of freezing it: ~1 iteration per chunk
/// on a full 4096² map, ~20 at 1024².
const TEXEL_ITERS_PER_CHUNK: u64 = 24_000_000;

/// Uniform buffer size — the WGSL `Params` struct (68 bytes) rounded up.
const PARAMS_SIZE: u64 = 80;

/// Map-readiness flag values (shared with the `map_async` callbacks).
const MAP_PENDING: u8 = 0;
const MAP_OK: u8 = 1;
const MAP_ERROR: u8 = 2;

/// Everything a run simulates from, borrowed at creation; heights (and the
/// optional mask) are domain-local, row-major, in meters.
pub(crate) struct SimInput<'a> {
    pub heights: &'a [f32],
    pub mask: Option<&'a [f32]>,
    pub dims: UVec2,
    /// Meters per texel.
    pub cell: f32,
    /// Toroidal wrap — only when the domain is a full looping map.
    pub wrap: bool,
    pub keep_maps: bool,
    pub settings: &'a ErosionSettings,
}

/// What a finished run hands back. `heights` is empty if the readback failed
/// (device loss) — the caller drops the run with a warning.
pub(crate) struct SimOutput {
    /// Final domain heights, meters.
    pub heights: Vec<f32>,
    /// Interleaved (wear, deposit) pairs, meters — when `keep_maps`.
    pub wear_deposit: Option<Vec<f32>>,
    /// Raw accumulated discharge per texel — when `keep_maps`.
    pub flow: Option<Vec<f32>>,
}

/// One in-flight GPU erosion run. Dropping it mid-run is safe: wgpu keeps the
/// resources alive until submitted work completes, and the map callbacks just
/// flip flags nobody reads anymore.
pub(crate) struct GpuErosion {
    device: RenderDevice,
    queue: RenderQueue,
    // Pipelines in dispatch order, plus the one-shot settle.
    flux: ComputePipeline,
    water: ComputePipeline,
    erode: ComputePipeline,
    advect: ComputePipeline,
    thermal: ComputePipeline,
    settle: ComputePipeline,
    // Per-family bind groups, indexed by iteration parity (see the shader's
    // banner: one layout per pass family keeps every layout within wgpu's
    // default 8-storage-buffer limit).
    flow_groups: [BindGroup; 2],
    sediment_groups: [BindGroup; 2],
    thermal_groups: [BindGroup; 2],
    heights: [Buffer; 2],
    accum_wd: Option<Buffer>,
    accum_flow: Option<Buffer>,
    staging_height: Buffer,
    staging_wd: Option<Buffer>,
    staging_flow: Option<Buffer>,
    dims: UVec2,
    total_iters: u32,
    done_iters: u32,
    /// Settle dispatched, staging copies submitted, maps requested.
    finalized: bool,
    map_height: Arc<AtomicU8>,
    map_wd: Option<Arc<AtomicU8>>,
    map_flow: Option<Arc<AtomicU8>>,
    /// Output already taken — the run is spent.
    delivered: bool,
}

impl GpuErosion {
    pub fn new(device: &RenderDevice, queue: &RenderQueue, input: &SimInput) -> Self {
        let texels = (input.dims.x * input.dims.y) as u64;
        let module = device.create_and_validate_shader_module(ShaderModuleDescriptor {
            label: Some("wilderness erosion"),
            source: ShaderSource::Wgsl(include_str!("erosion.wgsl").into()),
        });

        let storage = |binding: u32, read_only: bool| BindGroupLayoutEntry {
            binding,
            visibility: ShaderStages::COMPUTE,
            ty: BindingType::Buffer {
                ty: BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let uniform = BindGroupLayoutEntry {
            binding: 0,
            visibility: ShaderStages::COMPUTE,
            ty: BindingType::Buffer {
                ty: BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        // Binding indices match the WGSL module; each family lists exactly
        // the subset its passes statically use.
        let flow_layout = device.create_bind_group_layout(
            "erosion flow",
            &[
                uniform,
                storage(1, false), // height_in
                storage(3, false), // water
                storage(4, false), // flux_lr
                storage(5, false), // flux_tb
                storage(6, false), // vel
                storage(9, true),  // mask
            ],
        );
        let sediment_layout = device.create_bind_group_layout(
            "erosion sediment",
            &[
                uniform,
                storage(1, false),  // height_in
                storage(3, false),  // water
                storage(6, false),  // vel
                storage(7, false),  // sed_in
                storage(8, false),  // sed_out
                storage(9, true),   // mask
                storage(10, false), // accum_wd
                storage(11, false), // accum_flow
            ],
        );
        let thermal_layout = device.create_bind_group_layout(
            "erosion thermal",
            &[
                uniform,
                storage(1, false), // height_in
                storage(2, false), // height_out
                storage(9, true),  // mask
            ],
        );

        let pipeline = |label: &str, layout: &BindGroupLayout, entry: &str| {
            let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(layout)],
                ..Default::default()
            });
            device.create_compute_pipeline(&RawComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: PipelineCompilationOptions::default(),
                cache: None,
            })
        };
        let flux = pipeline("erosion flux", &flow_layout, "flux_pass");
        let water = pipeline("erosion water", &flow_layout, "water_pass");
        let erode = pipeline("erosion erode", &sediment_layout, "erode_pass");
        let advect = pipeline("erosion advect", &sediment_layout, "advect_pass");
        let thermal = pipeline("erosion thermal", &thermal_layout, "thermal_pass");
        let settle = pipeline("erosion settle", &sediment_layout, "settle_pass");

        let buffer = |label: &str, size: u64, usage: BufferUsages| {
            device.create_buffer(&BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let rw = BufferUsages::STORAGE | BufferUsages::COPY_DST;
        // wgpu zero-initializes buffers, so water/flux/sediment start dry.
        let heights = [
            buffer("erosion height a", texels * 4, rw | BufferUsages::COPY_SRC),
            buffer("erosion height b", texels * 4, rw | BufferUsages::COPY_SRC),
        ];
        let water_buf = buffer("erosion water", texels * 4, rw);
        let flux_lr = buffer("erosion flux lr", texels * 8, rw);
        let flux_tb = buffer("erosion flux tb", texels * 8, rw);
        let vel = buffer("erosion vel", texels * 8, rw);
        let sed = [
            buffer("erosion sed a", texels * 4, rw),
            buffer("erosion sed b", texels * 4, rw),
        ];
        // Dummy-sized when unused; the shader only indexes them behind the
        // masked/keep_maps flags (and WGSL indexing is clamped regardless).
        let mask = buffer(
            "erosion mask",
            input.mask.map_or(16, |m| m.len() as u64 * 4),
            rw,
        );
        let (accum_wd, accum_flow) = if input.keep_maps {
            (
                Some(buffer(
                    "erosion accum wd",
                    texels * 8,
                    rw | BufferUsages::COPY_SRC,
                )),
                Some(buffer(
                    "erosion accum flow",
                    texels * 4,
                    rw | BufferUsages::COPY_SRC,
                )),
            )
        } else {
            (None, None)
        };
        let accum_wd_bind = accum_wd
            .clone()
            .unwrap_or_else(|| buffer("erosion accum wd dummy", 16, rw));
        let accum_flow_bind = accum_flow
            .clone()
            .unwrap_or_else(|| buffer("erosion accum flow dummy", 16, rw));
        let params = buffer(
            "erosion params",
            PARAMS_SIZE,
            BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        );
        let map_read = BufferUsages::MAP_READ | BufferUsages::COPY_DST;
        let staging_height = buffer("erosion staging height", texels * 4, map_read);
        let staging_wd = input
            .keep_maps
            .then(|| buffer("erosion staging wd", texels * 8, map_read));
        let staging_flow = input
            .keep_maps
            .then(|| buffer("erosion staging flow", texels * 4, map_read));

        queue.write_buffer(&heights[0], 0, &f32_bytes(input.heights));
        if let Some(m) = input.mask {
            queue.write_buffer(&mask, 0, &f32_bytes(m));
        }
        queue.write_buffer(&params, 0, &pack_params(input));

        // Parity p: pass reads height/sediment `[p]`, thermal/advect write
        // `[1 - p]`.
        let flow_groups = [0usize, 1].map(|p| {
            device.create_bind_group(
                "erosion flow",
                &flow_layout,
                &[
                    entry(0, &params),
                    entry(1, &heights[p]),
                    entry(3, &water_buf),
                    entry(4, &flux_lr),
                    entry(5, &flux_tb),
                    entry(6, &vel),
                    entry(9, &mask),
                ],
            )
        });
        let sediment_groups = [0usize, 1].map(|p| {
            device.create_bind_group(
                "erosion sediment",
                &sediment_layout,
                &[
                    entry(0, &params),
                    entry(1, &heights[p]),
                    entry(3, &water_buf),
                    entry(6, &vel),
                    entry(7, &sed[p]),
                    entry(8, &sed[1 - p]),
                    entry(9, &mask),
                    entry(10, &accum_wd_bind),
                    entry(11, &accum_flow_bind),
                ],
            )
        });
        let thermal_groups = [0usize, 1].map(|p| {
            device.create_bind_group(
                "erosion thermal",
                &thermal_layout,
                &[
                    entry(0, &params),
                    entry(1, &heights[p]),
                    entry(2, &heights[1 - p]),
                    entry(9, &mask),
                ],
            )
        });

        Self {
            device: device.clone(),
            queue: queue.clone(),
            flux,
            water,
            erode,
            advect,
            thermal,
            settle,
            flow_groups,
            sediment_groups,
            thermal_groups,
            heights,
            accum_wd,
            accum_flow,
            staging_height,
            staging_wd,
            staging_flow,
            dims: input.dims,
            total_iters: input.settings.iterations.max(1),
            done_iters: 0,
            finalized: false,
            map_height: Arc::new(AtomicU8::new(MAP_PENDING)),
            map_wd: input.keep_maps.then(|| Arc::new(AtomicU8::new(MAP_PENDING))),
            map_flow: input.keep_maps.then(|| Arc::new(AtomicU8::new(MAP_PENDING))),
            delivered: false,
        }
    }

    /// Fraction of the run's iterations submitted (0..1) — the progress a UI
    /// bar shows; expect a short beat at 1.0 while the readback maps.
    pub fn progress(&self) -> f32 {
        self.done_iters as f32 / self.total_iters as f32
    }

    /// Encode and submit the next bounded chunk of iterations; after the last
    /// one, also the settle pass, the staging copies, and the map requests.
    /// No-op once finalized.
    pub fn step_chunk(&mut self) {
        if self.finalized {
            return;
        }
        let texels = (self.dims.x * self.dims.y) as u64;
        let chunk = ((TEXEL_ITERS_PER_CHUNK / texels.max(1)) as u32)
            .clamp(1, self.total_iters - self.done_iters);
        let groups_x = self.dims.x.div_ceil(WORKGROUP);
        let groups_y = self.dims.y.div_ceil(WORKGROUP);
        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("wilderness erosion"),
            });
        for _ in 0..chunk {
            let p = (self.done_iters % 2) as usize;
            let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
            pass.set_bind_group(0, &self.flow_groups[p], &[]);
            pass.set_pipeline(&self.flux);
            pass.dispatch_workgroups(groups_x, groups_y, 1);
            pass.set_pipeline(&self.water);
            pass.dispatch_workgroups(groups_x, groups_y, 1);
            pass.set_bind_group(0, &self.sediment_groups[p], &[]);
            pass.set_pipeline(&self.erode);
            pass.dispatch_workgroups(groups_x, groups_y, 1);
            pass.set_pipeline(&self.advect);
            pass.dispatch_workgroups(groups_x, groups_y, 1);
            pass.set_bind_group(0, &self.thermal_groups[p], &[]);
            pass.set_pipeline(&self.thermal);
            pass.dispatch_workgroups(groups_x, groups_y, 1);
            self.done_iters += 1;
        }
        if self.done_iters == self.total_iters {
            let p = (self.done_iters % 2) as usize;
            {
                let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
                pass.set_bind_group(0, &self.sediment_groups[p], &[]);
                pass.set_pipeline(&self.settle);
                pass.dispatch_workgroups(groups_x, groups_y, 1);
            }
            encoder.copy_buffer_to_buffer(&self.heights[p], 0, &self.staging_height, 0, texels * 4);
            if let (Some(src), Some(dst)) = (&self.accum_wd, &self.staging_wd) {
                encoder.copy_buffer_to_buffer(src, 0, dst, 0, texels * 8);
            }
            if let (Some(src), Some(dst)) = (&self.accum_flow, &self.staging_flow) {
                encoder.copy_buffer_to_buffer(src, 0, dst, 0, texels * 4);
            }
            self.finalized = true;
        }
        self.queue.0.submit([encoder.finish()]);
        if self.finalized {
            let request = |buffer: &Buffer, flag: &Arc<AtomicU8>| {
                let flag = flag.clone();
                buffer.slice(..).map_async(MapMode::Read, move |result| {
                    let state = if result.is_ok() { MAP_OK } else { MAP_ERROR };
                    flag.store(state, Ordering::Release);
                });
            };
            request(&self.staging_height, &self.map_height);
            if let (Some(buffer), Some(flag)) = (&self.staging_wd, &self.map_wd) {
                request(buffer, flag);
            }
            if let (Some(buffer), Some(flag)) = (&self.staging_flow, &self.map_flow) {
                request(buffer, flag);
            }
        }
    }

    /// Non-blocking: drive the device's callbacks and collect the result once
    /// every staging buffer has mapped. `heights` comes back empty if any map
    /// failed (device loss) — treat the run as aborted.
    pub fn poll_output(&mut self) -> Option<SimOutput> {
        if !self.finalized || self.delivered {
            return None;
        }
        let _ = self.device.poll(PollType::Poll);
        let flags = [
            Some(&self.map_height),
            self.map_wd.as_ref().into(),
            self.map_flow.as_ref().into(),
        ];
        let states: Vec<u8> = flags
            .into_iter()
            .flatten()
            .map(|f| f.load(Ordering::Acquire))
            .collect();
        if states.iter().any(|&s| s == MAP_PENDING) {
            return None;
        }
        self.delivered = true;
        if states.iter().any(|&s| s == MAP_ERROR) {
            return Some(SimOutput {
                heights: Vec::new(),
                wear_deposit: None,
                flow: None,
            });
        }
        let read = |buffer: &Buffer| {
            let data = {
                let range = buffer.slice(..).get_mapped_range();
                bytes_f32(&range)
            };
            buffer.unmap();
            data
        };
        Some(SimOutput {
            heights: read(&self.staging_height),
            wear_deposit: self.staging_wd.as_ref().map(&read),
            flow: self.staging_flow.as_ref().map(&read),
        })
    }

    /// Run to completion synchronously — the test harness's path (the editor
    /// itself always steps a chunk per frame instead).
    #[allow(dead_code)]
    pub fn block_until_done(&mut self) -> SimOutput {
        loop {
            self.step_chunk();
            let _ = self.device.poll(PollType::wait_indefinitely());
            if let Some(output) = self.poll_output() {
                return output;
            }
        }
    }
}

fn entry(binding: u32, buffer: &Buffer) -> BindGroupEntry<'_> {
    BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

/// Pack the WGSL `Params` uniform — field order and alignment must match the
/// struct in `erosion.wgsl` exactly.
fn pack_params(input: &SimInput) -> Vec<u8> {
    let s = input.settings;
    let dt = s.time_step.max(1e-3);
    let mut out = Vec::with_capacity(PARAMS_SIZE as usize);
    out.extend_from_slice(&(input.dims.x as i32).to_le_bytes());
    out.extend_from_slice(&(input.dims.y as i32).to_le_bytes());
    out.extend_from_slice(&(input.wrap as u32).to_le_bytes());
    out.extend_from_slice(&(input.mask.is_some() as u32).to_le_bytes());
    for f in [
        dt,
        input.cell,
        s.rain_rate,
        s.evaporation,
        s.capacity,
        s.dissolve_rate,
        s.deposit_rate,
        s.min_tilt_deg.to_radians().sin(),
        s.max_erosion_depth,
        s.talus_angle_deg.to_radians().tan(),
        s.thermal_rate,
    ] {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out.extend_from_slice(&(input.keep_maps as u32).to_le_bytes());
    out.extend_from_slice(&(0.9 * input.cell / dt).to_le_bytes());
    out.extend_from_slice(&s.max_flow_speed.to_le_bytes());
    out.resize(PARAMS_SIZE as usize, 0);
    out
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn bytes_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    /// The kernels are embedded source, compiled only when a run starts — so
    /// parse and validate them here, where plain `cargo test` catches a bad
    /// edit without needing any GPU.
    #[test]
    fn shader_parses_and_validates() {
        let module = naga::front::wgsl::parse_str(include_str!("erosion.wgsl"))
            .expect("erosion.wgsl must parse");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::default(),
        )
        .validate(&module)
        .expect("erosion.wgsl must validate");
    }
}
