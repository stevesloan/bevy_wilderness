//! The built-in erosion tool (D3): GPU shallow-water hydraulic erosion (the
//! Mei et al. 2007 "virtual pipes" model) plus a thermal (talus) pass, run as
//! compute shaders in bounded per-frame chunks so the app never freezes. A
//! run snapshots the field, simulates on the GPU, reads the result back, and
//! lands as height *deltas* through the normal dirty-sync path — so
//! re-quantize, [`TerrainRegionChanged`](crate::TerrainRegionChanged), the
//! re-bake debounce (which is what turns freshly carved cliffs rocky), and
//! undo (one run = one entry, D8) all follow for free. Deltas, not absolutes:
//! edits made while the run simulated survive instead of being stomped.
//!
//! A painted mask confines the run (D4): the simulation domain shrinks to the
//! padded mask bbox (bounding cost like droplet budgets used to), rain falls
//! only on masked texels, and every erode/deposit/talus amount is weighted by
//! the feathered mask value, so carved ground blends into untouched ground.
//! With no mask, the whole map erodes — and a full looping map simulates
//! toroidally, water and sediment crossing the seam (D6).
//!
//! The simulation works in world units (meters, seconds) on the GPU; see
//! `erosion.wgsl` for the kernels and `sim.rs` for the driver. Runs are
//! deterministic on a given device — fixed iteration count, no atomics, no
//! randomness (rain is uniform, not droplet-sampled). Realism knobs (D12) map
//! onto the model directly: capacity ∝ tilt × flow speed means flat ground
//! only receives sediment, deep pools armor their beds, and the continuous
//! thermal pass keeps carved walls at the angle of repose. With
//! [`ErosionSettings::keep_maps`], a run leaves its wear/deposit/discharge
//! analysis maps on the terrain as [`ErosionMaps`].

mod sim;

use bevy::{
    prelude::*,
    render::renderer::{RenderDevice, RenderQueue},
};

use crate::cursor::TerrainCursor;
use crate::gesture::{TerrainGesture, UndoBuffer};
use crate::settings::ErosionSettings;
use crate::terrain::EditableTerrain;
use crate::undo::UndoHistory;
use sim::{GpuErosion, SimInput, SimOutput};

/// Texels of slack around the mask bbox: room for water to run out of the
/// masked region, sediment to fan past its edge, and talus to creep.
const DOMAIN_PAD: u32 = 32;

/// Height deltas smaller than this (meters) are dropped — far below one R16
/// quantization step at any sane encode range, and it keeps a run's changed
/// bbox (and undo capture) tight instead of whole-map.
const DELTA_EPSILON: f32 = 1e-4;

/// Start an erosion run on a terrain: what the built-in erode tool writes on
/// click, and what a host UI's "Erode" button writes directly. Ignored while
/// that terrain already has an [`ErosionRun`] in flight.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErosionRequested {
    pub terrain: Entity,
}

/// Present on a terrain while its erosion run simulates on the GPU. A UI
/// reads [`progress`](Self::progress) for a progress bar; the component
/// disappears when the result has been applied.
#[derive(Component)]
pub struct ErosionRun {
    simulation: GpuErosion,
    /// Domain-local pre-run heights, for the delta diff.
    snapshot: Vec<f32>,
    /// The simulated texel rect within the full field.
    domain: URect,
    full_dims: UVec2,
    keep_maps: bool,
    outcome: Option<ErosionOutcome>,
}

/// Post-run analysis maps (D12), full-field row-major, replaced on each run.
/// Only present when [`ErosionSettings::keep_maps`] is set — host API for
/// e.g. splat or scatter rules; the editor itself never reads them.
#[derive(Component)]
pub struct ErosionMaps {
    /// Mask-weighted material eroded per texel, ≥ 0, meters.
    pub wear: Vec<f32>,
    /// Mask-weighted material deposited per texel, ≥ 0, meters.
    pub deposit: Vec<f32>,
    /// Log-normalized water discharge (∫ speed·depth·dt), 0..1 — the pipe
    /// model's flow-accumulation analogue: bright where drainage concentrated.
    pub flow: Vec<f32>,
    /// Map dimensions in texels.
    pub size: UVec2,
}

/// What a finished run lands.
struct ErosionOutcome {
    /// Height deltas in meters, full-field row-major, already mask-weighted.
    delta: Vec<f32>,
    /// Tight texel bbox of the non-negligible deltas; `None` if nothing
    /// changed (or the run aborted).
    changed: Option<URect>,
    maps: Option<ErosionMaps>,
}

impl ErosionRun {
    /// Fraction of the run's work done so far (0..1) — iterations simulated;
    /// expect a short beat at 1.0 while the result reads back and lands.
    pub fn progress(&self) -> f32 {
        if self.outcome.is_some() {
            1.0
        } else {
            self.simulation.progress()
        }
    }

    /// Snapshot `terrain` and build the GPU run: the simulation domain is the
    /// padded mask bbox (whole map when unmasked), toroidal only when that
    /// domain is a full looping map.
    fn start(
        terrain: &EditableTerrain,
        settings: &ErosionSettings,
        device: &RenderDevice,
        queue: &RenderQueue,
    ) -> Self {
        let field = &terrain.field;
        let full = field.full_rect();
        let dims = field.dimensions();
        let (domain, mask) = if terrain.mask_active() {
            let mask = terrain.mask_copy_rect(full);
            let mut min = UVec2::MAX;
            let mut max = UVec2::ZERO;
            for (i, &w) in mask.iter().enumerate() {
                if w > 0.0 {
                    let p = UVec2::new(i as u32 % dims.x, i as u32 / dims.x);
                    min = min.min(p);
                    max = max.max(p + 1);
                }
            }
            let domain = URect::new(
                min.x.saturating_sub(DOMAIN_PAD),
                min.y.saturating_sub(DOMAIN_PAD),
                (max.x + DOMAIN_PAD).min(dims.x),
                (max.y + DOMAIN_PAD).min(dims.y),
            );
            (domain, Some(terrain.mask_copy_rect(domain)))
        } else {
            (full, None)
        };
        let snapshot = field.copy_rect(domain);
        let simulation = GpuErosion::new(
            device,
            queue,
            &SimInput {
                heights: &snapshot,
                mask: mask.as_deref(),
                dims: domain.size(),
                cell: field.texel_size(),
                wrap: field.looping() && domain == full,
                keep_maps: settings.keep_maps,
                settings,
            },
        );
        Self {
            simulation,
            snapshot,
            domain,
            full_dims: dims,
            keep_maps: settings.keep_maps,
            outcome: None,
        }
    }

    /// Turn the GPU readback into a landable outcome: diff against the
    /// snapshot into a full-field delta buffer (thresholded, bbox-tracked)
    /// and assemble the analysis maps.
    fn ingest(&mut self, output: SimOutput) {
        if output.heights.is_empty() {
            warn!("bevy_wilderness_editor: erosion readback failed; run dropped");
            self.outcome = Some(ErosionOutcome {
                delta: Vec::new(),
                changed: None,
                maps: None,
            });
            return;
        }
        let full = (self.full_dims.x * self.full_dims.y) as usize;
        let width = self.domain.width();
        let mut delta = vec![0.0f32; full];
        let mut changed: Option<URect> = None;
        for (i, (&after, &before)) in output.heights.iter().zip(&self.snapshot).enumerate() {
            let d = after - before;
            if d.abs() < DELTA_EPSILON {
                continue;
            }
            let x = self.domain.min.x + i as u32 % width;
            let y = self.domain.min.y + i as u32 / width;
            delta[(y * self.full_dims.x + x) as usize] = d;
            changed = Some(match changed {
                None => URect::new(x, y, x + 1, y + 1),
                Some(b) => URect::new(
                    b.min.x.min(x),
                    b.min.y.min(y),
                    b.max.x.max(x + 1),
                    b.max.y.max(y + 1),
                ),
            });
        }
        let maps = self.keep_maps.then(|| {
            let mut maps = ErosionMaps {
                wear: vec![0.0; full],
                deposit: vec![0.0; full],
                flow: vec![0.0; full],
                size: self.full_dims,
            };
            let full_index = |i: usize| {
                let x = self.domain.min.x + i as u32 % width;
                let y = self.domain.min.y + i as u32 / width;
                (y * self.full_dims.x + x) as usize
            };
            if let Some(wd) = &output.wear_deposit {
                for (i, pair) in wd.chunks_exact(2).enumerate() {
                    let j = full_index(i);
                    maps.wear[j] = pair[0];
                    maps.deposit[j] = pair[1];
                }
            }
            if let Some(flow) = &output.flow {
                // Log, not linear: the trunk stream's max would zero out the
                // tributaries that make the network readable.
                let max = flow.iter().copied().fold(0.0f32, f32::max);
                let norm = (1.0 + max).ln();
                if norm > 0.0 {
                    for (i, &f) in flow.iter().enumerate() {
                        maps.flow[full_index(i)] = (1.0 + f).ln() / norm;
                    }
                }
            }
            maps
        });
        self.outcome = Some(ErosionOutcome {
            delta,
            changed,
            maps,
        });
    }
}

/// LMB starts a run over the mask (whole map if none). Runs in
/// `EditorSet::Tools`, gated on the erode tool being active.
pub(crate) fn request_on_click(
    buttons: Res<ButtonInput<MouseButton>>,
    cursor: Res<TerrainCursor>,
    runs: Query<(), With<ErosionRun>>,
    mut requests: MessageWriter<ErosionRequested>,
) {
    if !buttons.just_pressed(MouseButton::Left) {
        return;
    }
    let Some(hit) = cursor.0 else {
        return;
    };
    if runs.contains(hit.terrain) {
        return; // one run per terrain at a time
    }
    requests.write(ErosionRequested {
        terrain: hit.terrain,
    });
}

/// Snapshot each requested terrain and start its GPU run. Erosion needs the
/// renderer's device; in a headless app the requests are dropped with a
/// warning instead of piling up.
pub(crate) fn start_requested_runs(
    mut commands: Commands,
    mut requests: MessageReader<ErosionRequested>,
    settings: Res<ErosionSettings>,
    terrains: Query<(&EditableTerrain, Has<ErosionRun>)>,
    device: Option<Res<RenderDevice>>,
    queue: Option<Res<RenderQueue>>,
) {
    let (Some(device), Some(queue)) = (device, queue) else {
        if !requests.is_empty() {
            warn_once!("bevy_wilderness_editor: erosion needs the GPU (RenderDevice); ignoring");
            requests.clear();
        }
        return;
    };
    let mut started = Vec::new();
    for request in requests.read() {
        let Ok((terrain, running)) = terrains.get(request.terrain) else {
            continue;
        };
        if running || started.contains(&request.terrain) {
            continue;
        }
        started.push(request.terrain);
        let run = ErosionRun::start(terrain, &settings, &device, &queue);
        commands.entity(request.terrain).insert(run);
    }
}

/// Advance every in-flight run: submit the next bounded chunk of iterations
/// and collect the readback once it maps. The chunking is what keeps a
/// full-map 4096² run from freezing the frame — it degrades the frame rate
/// while it works instead.
pub(crate) fn drive_runs(mut runs: Query<&mut ErosionRun>) {
    for mut run in &mut runs {
        if run.outcome.is_some() {
            continue;
        }
        run.simulation.step_chunk();
        if let Some(output) = run.simulation.poll_output() {
            run.ingest(output);
        }
    }
}

/// Land finished runs: apply the deltas to the (possibly since-edited) field
/// as one undo entry and mark the changed region dirty — the sync in
/// `EditorSet::Apply` then re-quantizes, emits the event, and arms the
/// re-bake.
pub(crate) fn apply_finished_runs(
    mut commands: Commands,
    mut history: ResMut<UndoHistory>,
    mut gesture: ResMut<TerrainGesture>,
    mut terrains: Query<(Entity, &mut EditableTerrain, &mut ErosionRun)>,
) {
    for (entity, mut terrain, mut run) in &mut terrains {
        // Don't land mid-gesture: `begin` below would seal another tool's open
        // stroke and split its undo entry. The result waits a frame instead.
        if gesture.open() {
            continue;
        }
        if run.outcome.is_none() {
            continue;
        }
        let outcome = run.outcome.take().unwrap();
        commands.entity(entity).remove::<ErosionRun>();
        if let Some(maps) = outcome.maps {
            // Replaces any previous run's maps (D12).
            commands.entity(entity).insert(maps);
        }
        let Some(changed) = outcome.changed else {
            continue;
        };
        gesture.begin(&mut history, entity, "Erosion");
        gesture.capture(&terrain, UndoBuffer::Height, changed);
        let (min, max) = terrain.field.min_max();
        let width = terrain.field.dimensions().x;
        for y in changed.min.y..changed.max.y {
            for x in changed.min.x..changed.max.x {
                let d = outcome.delta[(y * width + x) as usize];
                if d != 0.0 {
                    let h = terrain.field.get(x as i64, y as i64);
                    terrain.field.set(x, y, (h + d).clamp(min, max));
                }
            }
        }
        terrain.mark_dirty(changed);
        gesture.seal(&mut history);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::TerrainField;
    use bevy::render::renderer::WgpuWrapper;
    use bevy::tasks::block_on;
    use std::sync::Arc;

    /// A headless device via raw wgpu (any backend, software fallback
    /// included). `None` skips the test on machines with no adapter at all.
    fn gpu() -> Option<(RenderDevice, RenderQueue)> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .inspect_err(|e| eprintln!("skipping erosion GPU test: no adapter ({e})"))
            .ok()?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .inspect_err(|e| eprintln!("skipping erosion GPU test: no device ({e})"))
            .ok()?;
        Some((
            RenderDevice::from(device),
            RenderQueue(Arc::new(WgpuWrapper::new(queue))),
        ))
    }

    /// 128² field with a central cone, ~150 m tall over ~40 texels.
    fn mound_terrain() -> EditableTerrain {
        let mut field = TerrainField::flat(128, 128, 1.0, 0.0, 400.0, false, 50.0);
        for y in 0..128 {
            for x in 0..128 {
                let d = Vec2::new(x as f32 - 64.0, y as f32 - 64.0).length();
                field.set(x, y, 50.0 + (150.0 - d * 3.75).max(0.0));
            }
        }
        EditableTerrain::new(field)
    }

    fn run_job(terrain: &EditableTerrain, settings: &ErosionSettings) -> Option<ErosionOutcome> {
        let (device, queue) = gpu()?;
        let mut run = ErosionRun::start(terrain, settings, &device, &queue);
        let output = run.simulation.block_until_done();
        run.ingest(output);
        Some(run.outcome.take().unwrap())
    }

    #[test]
    fn erosion_carves_and_deposits() {
        let terrain = mound_terrain();
        let Some(outcome) = run_job(&terrain, &ErosionSettings::default()) else {
            return;
        };
        let changed = outcome.changed.expect("erosion must change something");
        assert!(!changed.is_empty());
        let eroded: f32 = outcome.delta.iter().filter(|&&d| d < 0.0).sum();
        let deposited: f32 = outcome.delta.iter().filter(|&&d| d > 0.0).sum();
        assert!(eroded < -1.0, "must carve material: {eroded}");
        assert!(deposited > 1.0, "must deposit material: {deposited}");
        // The steep cone flank must lose material overall (carving dominates
        // over deposition there).
        let delta = &outcome.delta;
        let flank: f32 = (44..48)
            .flat_map(|x| (60..68).map(move |y| delta[y * 128 + x]))
            .sum();
        assert!(flank < 0.0, "steep flank should net-erode: {flank}");
    }

    /// On truly flat terrain, uniform rain must do *nothing*: no slope, no
    /// flux, no velocity, zero capacity — the GPU twin of the old droplet
    /// sim's shot-noise regression test.
    #[test]
    fn flat_terrain_untouched() {
        let field = TerrainField::flat(128, 128, 1.0, 0.0, 400.0, false, 50.0);
        let terrain = EditableTerrain::new(field);
        let Some(outcome) = run_job(&terrain, &ErosionSettings::default()) else {
            return;
        };
        assert!(
            outcome.changed.is_none(),
            "rain on flat ground must be a no-op: {:?}",
            outcome.changed
        );
    }

    /// Around a hill, the plain's texture must be deposition-dominated:
    /// sediment fans run out from the cone, and the only carving allowed is
    /// streams incising through their own deposits.
    #[test]
    fn flat_plain_gains_not_loses() {
        let terrain = mound_terrain();
        let Some(outcome) = run_job(&terrain, &ErosionSettings::default()) else {
            return;
        };
        let (mut neg_sum, mut pos_sum) = (0.0f32, 0.0f32);
        for y in 0..128u32 {
            for x in 0..128u32 {
                // Off the cone (base radius 40) with margin for fans and
                // talus at its foot.
                let d = Vec2::new(x as f32 - 64.0, y as f32 - 64.0).length();
                if d <= 55.0 {
                    continue;
                }
                let delta = outcome.delta[(y * 128 + x) as usize];
                if delta < 0.0 {
                    neg_sum -= delta;
                } else {
                    pos_sum += delta;
                }
            }
        }
        assert!(pos_sum > 1.0, "fans must reach the plain: {pos_sum}");
        assert!(
            neg_sum < 0.05 * pos_sum,
            "plain carving must be a sliver of deposition: -{neg_sum} vs +{pos_sum}"
        );
    }

    /// Same input, same device → bit-equal deltas: fixed iteration count,
    /// single-writer kernels, no randomness.
    #[test]
    fn same_input_same_result() {
        let terrain = mound_terrain();
        let settings = ErosionSettings::default();
        let (Some(a), Some(b)) = (run_job(&terrain, &settings), run_job(&terrain, &settings))
        else {
            return;
        };
        assert_eq!(a.changed, b.changed);
        assert_eq!(a.delta, b.delta);
    }

    #[test]
    fn mask_confines_the_run() {
        let mut terrain = mound_terrain();
        // Hard mask over the left half only.
        for y in 0..128 {
            for x in 0..60 {
                terrain.set_mask(x, y, 1.0);
            }
        }
        let Some(outcome) = run_job(&terrain, &ErosionSettings::default()) else {
            return;
        };
        assert!(outcome.changed.is_some());
        // Erosion and deposition are mask-weighted to zero outside; only the
        // thermal pass may push a one-texel scree fringe past the edge.
        let safe_x = 62;
        for y in 0..128 {
            for x in safe_x..128 {
                assert_eq!(
                    outcome.delta[y * 128 + x],
                    0.0,
                    "({x},{y}) outside the mask must not change"
                );
            }
        }
        // Inside the mask, material moved.
        let delta = &outcome.delta;
        let moved: f32 = (0..128)
            .flat_map(|y| (0..60).map(move |x| delta[y * 128 + x].abs()))
            .sum();
        assert!(moved > 1.0, "masked region must erode: {moved}");
    }

    #[test]
    fn thermal_relaxes_a_spike() {
        let mut field = TerrainField::flat(64, 64, 1.0, 0.0, 400.0, false, 100.0);
        field.set(32, 32, 300.0); // a 200 m single-texel spike
        let terrain = EditableTerrain::new(field);
        let settings = ErosionSettings {
            rain_rate: 0.0, // thermal only — no water, no hydraulic erosion
            ..default()
        };
        let Some(outcome) = run_job(&terrain, &settings) else {
            return;
        };
        assert!(outcome.changed.is_some());
        let spike = outcome.delta[32 * 64 + 32];
        let neighbor = outcome.delta[32 * 64 + 33];
        assert!(spike < -1.0, "spike must shed material: {spike}");
        assert!(neighbor > 0.0, "talus must land nearby: {neighbor}");
    }

    #[test]
    fn looping_water_crosses_the_seam() {
        // A ridge along the -X edge of a looping map: water flows downhill
        // across the seam and erodes both sides.
        let mut field = TerrainField::flat(64, 64, 1.0, 0.0, 400.0, true, 50.0);
        for y in 0..64 {
            for x in 0..64 {
                // Toroidal distance from the x = 0 column.
                let d = (x as i64).min((-(x as i64)).rem_euclid(64)) as f32;
                field.set(x, y, 50.0 + (100.0 - d * 12.0).max(0.0));
            }
        }
        let terrain = EditableTerrain::new(field);
        let Some(outcome) = run_job(&terrain, &ErosionSettings::default()) else {
            return;
        };
        let changed = outcome.changed.expect("must erode");
        assert!(!changed.is_empty());
        // Both sides of the seam saw movement.
        let left: f32 = (0..64).map(|y| outcome.delta[y * 64].abs()).sum();
        let right: f32 = (0..64).map(|y| outcome.delta[y * 64 + 63].abs()).sum();
        assert!(left > 0.0 && right > 0.0, "seam sides: {left} / {right}");
    }

    /// `keep_maps` retains populated analysis maps: wear on the flanks,
    /// deposits somewhere, discharge normalized into 0..1.
    #[test]
    fn keep_maps_retains_analysis() {
        let terrain = mound_terrain();
        let settings = ErosionSettings {
            keep_maps: true,
            ..default()
        };
        let Some(outcome) = run_job(&terrain, &settings) else {
            return;
        };
        let maps = outcome.maps.expect("keep_maps retains analysis maps");
        assert_eq!(maps.size, UVec2::splat(128));
        assert!(maps.wear.iter().any(|&w| w > 0.01), "wear must register");
        assert!(
            maps.deposit.iter().any(|&d| d > 0.01),
            "deposits must register"
        );
        let peak = maps.flow.iter().copied().fold(0.0f32, f32::max);
        assert!(
            peak > 0.5 && peak <= 1.0,
            "flow map must be log-normalized: {peak}"
        );
    }
}
