//! The built-in erosion tool (D3): droplet-based hydraulic erosion plus a
//! thermal (talus) pass, simulated on `AsyncComputeTaskPool` so the app never
//! freezes. A run snapshots the field, simulates in the background, and lands
//! as height *deltas* through the normal dirty-sync path — so re-quantize,
//! [`TerrainRegionChanged`](crate::TerrainRegionChanged), the re-bake debounce
//! (which is what turns freshly carved cliffs rocky), and undo (one run = one
//! entry, D8) all follow for free.
//!
//! A painted mask confines the run (D4): droplets spawn inside it and every
//! delta is weighted by the feathered mask value, so carved ground blends into
//! untouched ground. With no mask, the whole map erodes.
//!
//! The simulation works in *normalized* height units (0..1 of the encode
//! range), so the standard droplet parameters keep their meaning regardless of
//! world scale. Droplets read wrap-aware samples, so on looping terrain a
//! droplet exiting one edge re-enters the opposite (D6).
//!
//! Realism rules (D12): carrying capacity is proportional to slope — flat
//! ground never carves, it only receives sediment; droplets warm up before
//! they may erode (no spawn-point pitting) and die where they stagnate (no
//! random-walk scratches); a per-round D8 flow-accumulation pass boosts
//! capacity where drainage concentrates (connected dendritic channels);
//! deposits spread over a falloff brush and each round's deposit accumulator
//! is box-blurred — carving stays crisp, fans and valley fill land smooth.
//! With [`ErosionSettings::keep_maps`], a run leaves its wear/deposit/flow
//! analysis maps on the terrain as [`ErosionMaps`].

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use bevy::{
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task, TaskPool, block_on, poll_once},
};

use crate::cursor::TerrainCursor;
use crate::gesture::{TerrainGesture, UndoBuffer};
use crate::settings::ErosionSettings;
use crate::terrain::EditableTerrain;
use crate::undo::UndoHistory;

/// Simulation rounds per run. Batches within a round parallelize over
/// per-batch delta buffers (droplet writes scatter, so they can't share one —
/// D3), then the summed round is applied to the working heights: later rounds'
/// droplets follow the channels earlier rounds carved, which is where the
/// dendritic feedback comes from.
const ROUNDS: u32 = 8;

/// Cap on parallel droplet batches — each owns a full-field f32 delta buffer
/// (67 MB at 4096²), so beyond this extra threads would buy memory, not time.
const MAX_BATCHES: usize = 4;

/// Steps a freshly spawned droplet flows before it may carve — kills the
/// spawn-point shot noise of uniform rain (D12).
const DROPLET_WARMUP_STEPS: u32 = 2;

const SQRT_2: f32 = std::f32::consts::SQRT_2;

/// The D8 stencil with per-step horizontal distance, shared by the thermal
/// pass (steepest *uphill* excess) and flow accumulation (steepest descent).
const NEIGHBORS: [(i32, i32, f32); 8] = [
    (-1, -1, SQRT_2),
    (0, -1, 1.0),
    (1, -1, SQRT_2),
    (-1, 0, 1.0),
    (1, 0, 1.0),
    (-1, 1, SQRT_2),
    (0, 1, 1.0),
    (1, 1, SQRT_2),
];

/// Start an erosion run on a terrain: what the built-in erode tool writes on
/// click, and what a host UI's "Erode" button writes directly. Ignored while
/// that terrain already has an [`ErosionRun`] in flight.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErosionRequested {
    pub terrain: Entity,
}

/// Present on a terrain while its erosion run simulates in the background.
/// A UI reads [`progress`](Self::progress) for a progress bar; the component
/// disappears when the result has been applied.
#[derive(Component)]
pub struct ErosionRun {
    task: Task<ErosionOutcome>,
    work_done: Arc<AtomicU32>,
    work_total: u32,
}

impl ErosionRun {
    /// Fraction of the run's work done so far (0..1) — droplets simulated
    /// plus per-round flow passes (D12). The thermal pass runs after the last
    /// round, so expect a short beat at 1.0 before the result lands.
    pub fn progress(&self) -> f32 {
        self.work_done.load(Ordering::Relaxed) as f32 / self.work_total.max(1) as f32
    }
}

/// Post-run analysis maps (D12), full-field row-major, replaced on each run.
/// Only present when [`ErosionSettings::keep_maps`] is set — host API for
/// e.g. splat or scatter rules; the editor itself never reads them. Values
/// are pre-clamp components, so where the encode range clamped the applied
/// delta they slightly overstate.
#[derive(Component)]
pub struct ErosionMaps {
    /// Mask-weighted material eroded per texel, ≥ 0, meters.
    pub wear: Vec<f32>,
    /// Mask-weighted material deposited per texel, ≥ 0, meters.
    pub deposit: Vec<f32>,
    /// The last round's log-normalized flow accumulation, 0..1 (all zero if
    /// the flow pass was disabled).
    pub flow: Vec<f32>,
    /// Map dimensions in texels.
    pub size: UVec2,
}

/// What the background task returns.
struct ErosionOutcome {
    /// Height deltas in meters, full-field row-major, already mask-weighted.
    delta: Vec<f32>,
    /// Tight texel bbox of the non-zero deltas; `None` if nothing changed.
    changed: Option<URect>,
    /// Analysis maps, when [`ErosionSettings::keep_maps`] asked for them.
    maps: Option<ErosionMaps>,
}

/// One batch's scattered droplet writes, split so deposition can be smoothed
/// independently of carving (D12).
struct BatchDelta {
    /// Carved material, ≤ 0 per texel.
    erode: Vec<f32>,
    /// Deposited material, ≥ 0 per texel.
    deposit: Vec<f32>,
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

/// Snapshot each requested terrain and spawn its simulation task.
pub(crate) fn start_requested_runs(
    mut commands: Commands,
    mut requests: MessageReader<ErosionRequested>,
    settings: Res<ErosionSettings>,
    terrains: Query<(&EditableTerrain, Has<ErosionRun>)>,
) {
    let mut started = Vec::new();
    for request in requests.read() {
        let Ok((terrain, running)) = terrains.get(request.terrain) else {
            continue;
        };
        if running || started.contains(&request.terrain) {
            continue;
        }
        started.push(request.terrain);
        let job = ErosionJob::new(terrain, &settings);
        let work_done = job.done.clone();
        let work_total = job.work_total;
        let task = AsyncComputeTaskPool::get_or_init(TaskPool::default).spawn(job.run_async());
        commands.entity(request.terrain).insert(ErosionRun {
            task,
            work_done,
            work_total,
        });
    }
}

/// Land finished runs: apply the deltas to the (possibly since-edited) field as
/// one undo entry and mark the changed region dirty — the sync in
/// `EditorSet::Apply` then re-quantizes, emits the event, and arms the re-bake.
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
        let Some(outcome) = block_on(poll_once(&mut run.task)) else {
            continue;
        };
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
        // Deltas, not absolute heights: edits made while the run simulated
        // survive instead of being stomped by the snapshot.
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

/// Everything the background task needs, snapshotted at request time.
struct ErosionJob {
    /// Working heights, normalized to 0..1 of the encode range.
    heights: Vec<f32>,
    /// The feathered 0..1 mask; `None` = unmasked (whole map erodes).
    mask: Option<Vec<f32>>,
    /// Texel indices with non-zero mask weight — droplet spawn candidates.
    spawn: Option<Vec<u32>>,
    width: u32,
    height: u32,
    looping: bool,
    settings: ErosionSettings,
    /// Meters per normalized height unit (the encode range span).
    height_scale: f32,
    /// Talus height threshold per texel of horizontal distance, normalized.
    talus: f32,
    /// Slope (normalized height per texel) below which erosion fades out
    /// (D12) — the per-job form of [`ErosionSettings::min_slope_deg`].
    min_slope: f32,
    droplets: u32,
    /// Progress units one flow-accumulation pass is worth (0 when disabled) —
    /// real work the bar must cover (D12).
    flow_units: u32,
    /// Total progress units: droplets plus every flow pass that will run.
    work_total: u32,
    seed: u64,
    done: Arc<AtomicU32>,
}

impl ErosionJob {
    fn new(terrain: &EditableTerrain, settings: &ErosionSettings) -> Self {
        let field = &terrain.field;
        let dims = field.dimensions();
        let (min, max) = field.min_max();
        let height_scale = max - min;
        let mut heights = field.copy_rect(field.full_rect());
        for h in &mut heights {
            *h = (*h - min) / height_scale;
        }
        // The budget is a *density*: droplets scale with the eroded area (the
        // mask, or the whole map), so a run feels the same at any resolution
        // and masking bounds cost (D3).
        let budget = |texels: usize| (texels as f32 * settings.droplet_density) as u32;
        let (mask, spawn, droplets) = if terrain.mask_active() {
            let mask = terrain.mask_copy_rect(field.full_rect());
            let spawn: Vec<u32> = mask
                .iter()
                .enumerate()
                .filter(|&(_, &w)| w > 0.0)
                .map(|(i, _)| i as u32)
                .collect();
            let droplets = budget(spawn.len());
            (Some(mask), Some(spawn), droplets)
        } else {
            let droplets = budget(heights.len());
            (None, None, droplets)
        };
        let talus = settings.talus_angle_deg.to_radians().tan() * field.texel_size() / height_scale;
        let min_slope =
            settings.min_slope_deg.to_radians().tan() * field.texel_size() / height_scale;
        // A flow pass ≈ a quarter-round of droplet work (sort-dominated);
        // count it so the bar doesn't stall on flow rounds. Rounds whose
        // droplet quota is zero skip the flow pass too.
        let flow_units = if settings.flow_strength > 0.0 && droplets > 0 {
            (droplets / (ROUNDS * 4)).max(1)
        } else {
            0
        };
        let work_total = droplets + droplets.min(ROUNDS) * flow_units;
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5eed);
        Self {
            heights,
            mask,
            spawn,
            width: dims.x,
            height: dims.y,
            looping: field.looping(),
            settings: settings.clone(),
            height_scale,
            talus,
            min_slope,
            droplets,
            flow_units,
            work_total,
            seed,
            done: Arc::new(AtomicU32::new(0)),
        }
    }

    async fn run_async(self) -> ErosionOutcome {
        self.run()
    }

    fn run(mut self) -> ErosionOutcome {
        let len = self.heights.len();
        // Net normalized delta vs. the snapshot — what the run returns.
        let mut total = vec![0.0f32; len];
        let brush = erosion_brush(self.settings.erosion_radius);
        let deposit_brush = erosion_brush(self.settings.deposit_radius.max(1));
        let pool = AsyncComputeTaskPool::get_or_init(TaskPool::default);
        let batches = pool.thread_num().clamp(1, MAX_BATCHES);
        // Round accumulators (reused): batch sums land here so this round's
        // deposits can be smoothed before they merge (D12).
        let mut erode_round = vec![0.0f32; len];
        let mut deposit_round = vec![0.0f32; len];
        let mut scratch = vec![0.0f32; len];
        let mut maps = self.settings.keep_maps.then(|| ErosionMaps {
            wear: vec![0.0; len],
            deposit: vec![0.0; len],
            flow: vec![0.0; len],
            size: UVec2::new(self.width, self.height),
        });
        for round in 0..ROUNDS {
            let quota = self.droplets * (round + 1) / ROUNDS - self.droplets * round / ROUNDS;
            if quota == 0 {
                continue;
            }
            // Recompute drainage over this round's heights (D12): flow
            // re-concentrates into the channels the previous round carved,
            // which is what makes the network dendritic.
            let flow = if self.settings.flow_strength > 0.0 {
                let mut acc = self.flow_accumulation();
                let max = acc.iter().copied().fold(1.0f32, f32::max);
                let norm = (1.0 + max).ln();
                for a in &mut acc {
                    // Log, not a power: the trunk stream's max would zero
                    // out the tributaries that make the network readable.
                    *a = (1.0 + *a).ln() / norm;
                }
                self.done.fetch_add(self.flow_units, Ordering::Relaxed);
                Some(acc)
            } else {
                None
            };
            let job = &self;
            let brush = &brush;
            let deposit_brush = &deposit_brush;
            let flow = flow.as_deref();
            let deltas = pool.scope(|scope| {
                for batch in 0..batches as u32 {
                    let count =
                        quota * (batch + 1) / batches as u32 - quota * batch / batches as u32;
                    if count == 0 {
                        continue;
                    }
                    // Distinct, deterministic stream per (round, batch).
                    let seed = (job.seed ^ ((round as u64) << 32 | batch as u64))
                        .wrapping_mul(0x2545F4914F6CDD1D);
                    scope.spawn(async move {
                        job.simulate_batch(count, seed, brush, deposit_brush, flow)
                    });
                }
            });
            // Sum the batch buffers in spawn order — a fixed f32 summation
            // order is what keeps runs deterministic (§10); never
            // parallelize this merge.
            erode_round.fill(0.0);
            deposit_round.fill(0.0);
            for batch in &deltas {
                for i in 0..len {
                    erode_round[i] += batch.erode[i];
                    deposit_round[i] += batch.deposit[i];
                }
            }
            // Smooth this round's *deposits only* (D12): fans and valley
            // fill land soft while channel walls stay crisp. Before mask
            // weighting, so the mask still hard-confines the result (§10).
            if self.settings.deposit_blur_radius > 0 {
                blur_separable(
                    &mut deposit_round,
                    &mut scratch,
                    self.width,
                    self.height,
                    self.settings.deposit_blur_radius,
                    self.looping,
                );
            }
            // Apply the round, weighted by the feathered mask (D4), so the
            // next round's droplets flow over the carved terrain.
            for i in 0..len {
                let e = erode_round[i];
                let d = deposit_round[i];
                if e == 0.0 && d == 0.0 {
                    continue;
                }
                let weight = self.mask.as_ref().map_or(1.0, |m| m[i]);
                if weight <= 0.0 {
                    continue;
                }
                let new = (self.heights[i] + (e + d) * weight).clamp(0.0, 1.0);
                total[i] += new - self.heights[i];
                self.heights[i] = new;
                if let Some(maps) = &mut maps {
                    maps.wear[i] -= e * weight;
                    maps.deposit[i] += d * weight;
                }
            }
            if let (Some(maps), Some(flow)) = (&mut maps, flow) {
                maps.flow.copy_from_slice(flow);
            }
        }
        self.thermal(&mut total);
        // Convert to meters and find the tight bbox of what actually changed.
        let mut changed: Option<URect> = None;
        for (i, d) in total.iter_mut().enumerate() {
            if *d == 0.0 {
                continue;
            }
            *d *= self.height_scale;
            let (x, y) = (i as u32 % self.width, i as u32 / self.width);
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
        // The analysis maps convert to meters like the deltas do.
        if let Some(maps) = &mut maps {
            for w in &mut maps.wear {
                *w *= self.height_scale;
            }
            for d in &mut maps.deposit {
                *d *= self.height_scale;
            }
        }
        ErosionOutcome {
            delta: total,
            changed,
            maps,
        }
    }

    /// Simulate `count` droplets against the round's start-of-round heights,
    /// scattering erode/deposit writes into this batch's own split buffers.
    /// `flow` is the round's normalized flow-accumulation map, if enabled.
    fn simulate_batch(
        &self,
        count: u32,
        seed: u64,
        brush: &[(IVec2, f32)],
        deposit_brush: &[(IVec2, f32)],
        flow: Option<&[f32]>,
    ) -> BatchDelta {
        let mut delta = BatchDelta {
            erode: vec![0.0f32; self.heights.len()],
            deposit: vec![0.0f32; self.heights.len()],
        };
        let mut rng = Pcg32::new(seed);
        let s = &self.settings;
        for _ in 0..count {
            let mut pos = self.spawn_position(&mut rng);
            let mut dir = Vec2::ZERO;
            let mut speed = 1.0f32;
            let mut water = 1.0f32;
            let mut sediment = 0.0f32;
            for step in 0..s.max_lifetime {
                let cell = pos.floor().as_ivec2();
                let (grad, h_old) = self.gradient_height(pos);
                // Momentum blend: inertia 0 hugs the gradient, 1 never turns.
                dir = dir * s.inertia - grad * (1.0 - s.inertia);
                let len = dir.length();
                if len <= 1e-6 {
                    // No momentum *and* no gradient: stagnant. Die where it
                    // stands (the terminal deposit below leaves the load) —
                    // wandering randomly would carve scratches into flat
                    // ground (D12).
                    break;
                }
                dir /= len;
                pos += dir;
                if self.looping {
                    // Exiting one edge re-enters the opposite (D6).
                    pos.x = pos.x.rem_euclid(self.width as f32);
                    pos.y = pos.y.rem_euclid(self.height as f32);
                } else if pos.x < 0.0
                    || pos.y < 0.0
                    || pos.x >= (self.width - 1) as f32
                    || pos.y >= (self.height - 1) as f32
                {
                    // Flowed off a finite map; its load leaves with it.
                    sediment = 0.0;
                    break;
                }
                let dh = self.height_at(pos) - h_old;
                // Carrying capacity is proportional to slope (D12): flat
                // ground gives ≈ 0 capacity, so laden droplets deposit and
                // unladen ones do nothing — the old capacity floor carved
                // shot noise into plains.
                let slope = (-dh).max(0.0);
                // Drainage boost (D12): rain falls everywhere, erosive power
                // concentrates where flow accumulates — connected channels,
                // not scattered scratches.
                let boost = flow
                    .and_then(|f| self.index(cell).map(|i| 1.0 + s.flow_strength * f[i]))
                    .unwrap_or(1.0);
                let capacity = slope * speed * water * s.sediment_capacity * boost;
                if sediment > capacity || dh > 0.0 {
                    // Over capacity (or ran uphill into a pit wall): deposit
                    // into the cell just left. Filling to the uphill step
                    // smooths pits instead of overshooting them.
                    let amount = if dh > 0.0 {
                        sediment.min(dh)
                    } else {
                        (sediment - capacity) * s.deposit_rate
                    };
                    sediment -= amount;
                    self.deposit(&mut delta.deposit, cell, amount, deposit_brush);
                } else if step >= DROPLET_WARMUP_STEPS {
                    // Under capacity: erode, spread over the brush so channels
                    // don't collapse into single-texel trenches. The low-slope
                    // gate fades carving out toward flat ground; never take
                    // more than the drop, or flow would cut below its
                    // destination.
                    let gate = if self.min_slope > 0.0 {
                        (slope / self.min_slope).min(1.0)
                    } else {
                        1.0
                    };
                    let amount = ((capacity - sediment) * s.erode_rate * gate).min(slope);
                    sediment += amount;
                    for &(off, weight) in brush {
                        if let Some(i) = self.index(cell + off) {
                            delta.erode[i] -= amount * weight;
                        }
                    }
                }
                // Downhill drop converts to speed; uphill bleeds it off.
                speed = (speed * speed - dh * s.gravity).max(0.0).sqrt();
                water *= 1.0 - s.evaporate_rate;
                if water < 1e-3 {
                    break;
                }
            }
            // The droplet dies where it stands; leave its load there.
            if sediment > 0.0 {
                self.deposit(
                    &mut delta.deposit,
                    pos.floor().as_ivec2(),
                    sediment,
                    deposit_brush,
                );
            }
            self.done.fetch_add(1, Ordering::Relaxed);
        }
        delta
    }

    /// The thermal (talus) pass (D3): each iteration, material on slopes
    /// steeper than the angle of repose sheds toward the steepest lower
    /// neighbor, mask-weighted at the source so scree feathers at the mask
    /// edge. Bounded to the masked region (talus creeps one texel per
    /// iteration) when a mask is painted.
    fn thermal(&mut self, total: &mut [f32]) {
        let s = &self.settings;
        if s.thermal_iterations == 0 || s.thermal_rate <= 0.0 {
            return;
        }
        let (w, h) = (self.width as i32, self.height as i32);
        let (x0, y0, x1, y1) = match &self.spawn {
            Some(spawn) => {
                let mut min = IVec2::MAX;
                let mut max = IVec2::MIN;
                for &i in spawn {
                    let p = IVec2::new((i % self.width) as i32, (i / self.width) as i32);
                    min = min.min(p);
                    max = max.max(p);
                }
                let pad = s.thermal_iterations as i32 + 1;
                (
                    (min.x - pad).max(0),
                    (min.y - pad).max(0),
                    (max.x + 1 + pad).min(w),
                    (max.y + 1 + pad).min(h),
                )
            }
            None => (0, 0, w, h),
        };
        let mut transfer = vec![0.0f32; self.heights.len()];
        for _ in 0..s.thermal_iterations {
            transfer.fill(0.0);
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) as usize;
                    let mask_weight = self.mask.as_ref().map_or(1.0, |m| m[i]);
                    if mask_weight <= 0.0 {
                        continue;
                    }
                    let hh = self.heights[i];
                    // Steepest neighbor whose drop exceeds the talus angle.
                    let mut best: Option<(f32, usize)> = None;
                    for (dx, dy, dist) in NEIGHBORS {
                        let Some(j) = self.index(IVec2::new(x + dx, y + dy)) else {
                            continue;
                        };
                        let excess = hh - self.heights[j] - self.talus * dist;
                        if excess > 0.0 && best.is_none_or(|(e, _)| excess > e) {
                            best = Some((excess, j));
                        }
                    }
                    if let Some((excess, j)) = best {
                        // Half the excess would level the pair to the angle of
                        // repose; the rate is how much of that moves per
                        // iteration.
                        let m = 0.5 * s.thermal_rate * excess * mask_weight;
                        transfer[i] -= m;
                        transfer[j] += m;
                    }
                }
            }
            // Whole-buffer apply: wrapped transfers on looping terrain can
            // land anywhere, and it's a cheap bandwidth-bound sweep.
            for (i, &t) in transfer.iter().enumerate() {
                if t != 0.0 {
                    let new = (self.heights[i] + t).clamp(0.0, 1.0);
                    total[i] += new - self.heights[i];
                    self.heights[i] = new;
                }
            }
        }
    }

    /// D8 flow accumulation over the current working heights (D12): every
    /// texel gets one unit of rain; each cell passes its accumulated total to
    /// its steepest-descent D8 neighbor, processed high-to-low so upstream
    /// sums arrive before downstream. Pits and plateau minima keep what
    /// reaches them. Wrap-aware via [`Self::index`], so drainage crosses the
    /// looping seam (D6). Returns *raw* accumulation (≥ 1 everywhere); the
    /// caller log-normalizes.
    fn flow_accumulation(&self) -> Vec<f32> {
        let len = self.heights.len();
        // High-to-low processing order. ⚠️ `sort_unstable` on equal-height
        // plateaus would route nondeterministically without the index
        // tie-break (§10 determinism gotcha).
        let mut order: Vec<u32> = (0..len as u32).collect();
        order.sort_unstable_by(|&a, &b| {
            self.heights[b as usize]
                .total_cmp(&self.heights[a as usize])
                .then(a.cmp(&b))
        });
        let mut acc = vec![1.0f32; len];
        for &i in &order {
            let (x, y) = ((i % self.width) as i32, (i / self.width) as i32);
            let hh = self.heights[i as usize];
            // Steepest-descent neighbor receives everything.
            let mut best: Option<(f32, usize)> = None;
            for (dx, dy, dist) in NEIGHBORS {
                let Some(j) = self.index(IVec2::new(x + dx, y + dy)) else {
                    continue;
                };
                let drop = (hh - self.heights[j]) / dist;
                if drop > 0.0 && best.is_none_or(|(d, _)| drop > d) {
                    best = Some((drop, j));
                }
            }
            if let Some((_, j)) = best {
                acc[j] += acc[i as usize];
            }
        }
        acc
    }

    /// Where a droplet is born: uniform over the map, or — when masked —
    /// uniform over masked texels, rejection-thinned by the feathered weight so
    /// spawn density tapers with the mask edge.
    fn spawn_position(&self, rng: &mut Pcg32) -> Vec2 {
        match (&self.spawn, &self.mask) {
            (Some(spawn), Some(mask)) => loop {
                let i = spawn[rng.range(spawn.len() as u32) as usize] as usize;
                if rng.next_f32() < mask[i] {
                    let (x, y) = (i as u32 % self.width, i as u32 / self.width);
                    return Vec2::new(x as f32 + rng.next_f32(), y as f32 + rng.next_f32());
                }
            },
            _ => Vec2::new(
                rng.next_f32() * self.width as f32,
                rng.next_f32() * self.height as f32,
            ),
        }
    }

    /// Texel index for possibly-out-of-range coordinates: wraps when looping,
    /// `None` (write dropped) beyond a finite edge — the same conventions as
    /// [`TerrainField::wrap_texel`](crate::TerrainField::wrap_texel).
    fn index(&self, p: IVec2) -> Option<usize> {
        let (w, h) = (self.width as i32, self.height as i32);
        let (x, y) = if self.looping {
            (p.x.rem_euclid(w), p.y.rem_euclid(h))
        } else if p.x >= 0 && p.x < w && p.y >= 0 && p.y < h {
            (p.x, p.y)
        } else {
            return None;
        };
        Some((y * w + x) as usize)
    }

    /// One texel's working height: wraps when looping, edge-clamps otherwise
    /// (matching `TerrainField::get`).
    fn at(&self, x: i32, y: i32) -> f32 {
        let (w, h) = (self.width as i32, self.height as i32);
        let (x, y) = if self.looping {
            (x.rem_euclid(w), y.rem_euclid(h))
        } else {
            (x.clamp(0, w - 1), y.clamp(0, h - 1))
        };
        self.heights[(y * w + x) as usize]
    }

    /// Bilinear height and its gradient at fractional texel `pos`.
    fn gradient_height(&self, pos: Vec2) -> (Vec2, f32) {
        let cell = pos.floor();
        let (x, y) = (cell.x as i32, cell.y as i32);
        let f = pos - cell;
        let h00 = self.at(x, y);
        let h10 = self.at(x + 1, y);
        let h01 = self.at(x, y + 1);
        let h11 = self.at(x + 1, y + 1);
        let grad = Vec2::new(
            (h10 - h00) * (1.0 - f.y) + (h11 - h01) * f.y,
            (h01 - h00) * (1.0 - f.x) + (h11 - h10) * f.x,
        );
        let height = h00 * (1.0 - f.x) * (1.0 - f.y)
            + h10 * f.x * (1.0 - f.y)
            + h01 * (1.0 - f.x) * f.y
            + h11 * f.x * f.y;
        (grad, height)
    }

    fn height_at(&self, pos: Vec2) -> f32 {
        self.gradient_height(pos).1
    }

    /// Spread a deposit over the falloff brush centered on `cell` — the
    /// erosion brush's mirror (D12), so sediment lands as a smooth mound
    /// instead of a single-texel bump.
    fn deposit(&self, delta: &mut [f32], cell: IVec2, amount: f32, brush: &[(IVec2, f32)]) {
        for &(off, weight) in brush {
            if let Some(i) = self.index(cell + off) {
                delta[i] += amount * weight;
            }
        }
    }
}

/// Separable wrap-aware box blur, used on each round's deposit accumulator
/// (D12). Single-threaded on purpose: a parallel blur would reintroduce
/// f32-order nondeterminism (§10) for a pass that's bandwidth-bound anyway.
/// Finite maps renormalize the truncated kernel by the in-bounds tap count,
/// conserving mass at edges instead of smearing the clamp value.
fn blur_separable(
    buf: &mut [f32],
    scratch: &mut [f32],
    width: u32,
    height: u32,
    radius: u32,
    looping: bool,
) {
    let (w, h, r) = (width as i32, height as i32, radius as i32);
    // Horizontal pass into scratch.
    for y in 0..h {
        let row = (y * w) as usize;
        for x in 0..w {
            let mut sum = 0.0;
            let mut taps = 0u32;
            for dx in -r..=r {
                let sx = if looping {
                    (x + dx).rem_euclid(w)
                } else {
                    x + dx
                };
                if sx < 0 || sx >= w {
                    continue;
                }
                sum += buf[row + sx as usize];
                taps += 1;
            }
            scratch[row + x as usize] = sum / taps as f32;
        }
    }
    // Vertical pass back into buf.
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0;
            let mut taps = 0u32;
            for dy in -r..=r {
                let sy = if looping {
                    (y + dy).rem_euclid(h)
                } else {
                    y + dy
                };
                if sy < 0 || sy >= h {
                    continue;
                }
                sum += scratch[(sy * w + x) as usize];
                taps += 1;
            }
            buf[(y * w + x) as usize] = sum / taps as f32;
        }
    }
}

/// Precomputed erosion brush: offsets within `radius` texels, linear-falloff
/// weights summing to 1. Spreading each erode step over the brush is what
/// keeps channels from collapsing into single-texel pits (D3).
fn erosion_brush(radius: u32) -> Vec<(IVec2, f32)> {
    let r = radius.max(1) as i32;
    let mut cells = Vec::new();
    let mut sum = 0.0;
    for dy in -r..=r {
        for dx in -r..=r {
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            let weight = 1.0 - d / r as f32;
            if weight > 0.0 {
                cells.push((IVec2::new(dx, dy), weight));
                sum += weight;
            }
        }
    }
    for cell in &mut cells {
        cell.1 /= sum;
    }
    cells
}

/// Minimal PCG32 (O'Neill) — deterministic, seedable, dependency-free; each
/// droplet batch gets its own stream so parallel batches don't correlate.
struct Pcg32 {
    state: u64,
}

impl Pcg32 {
    fn new(seed: u64) -> Self {
        let mut rng = Self {
            state: seed.wrapping_add(0x9E3779B97F4A7C15),
        };
        rng.next_u32();
        rng
    }

    fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        xorshifted.rotate_right((old >> 59) as u32)
    }

    /// Uniform in [0, 1).
    fn next_f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (1.0 / 16_777_216.0)
    }

    /// Uniform in [0, n).
    fn range(&mut self, n: u32) -> u32 {
        ((self.next_u32() as u64 * n as u64) >> 32) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::TerrainField;

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

    fn test_settings() -> ErosionSettings {
        ErosionSettings {
            // ~20 k droplets on the 128² mound — dense enough to assert on.
            droplet_density: 1.25,
            ..default()
        }
    }

    fn run_job(terrain: &EditableTerrain, settings: &ErosionSettings) -> ErosionOutcome {
        let mut job = ErosionJob::new(terrain, settings);
        job.seed = 42; // deterministic tests
        job.run()
    }

    #[test]
    fn erosion_carves_and_deposits() {
        let terrain = mound_terrain();
        let settings = test_settings();
        let outcome = run_job(&terrain, &settings);
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

    #[test]
    fn applied_deltas_stay_in_encode_range() {
        let terrain = mound_terrain();
        let settings = test_settings();
        let outcome = run_job(&terrain, &settings);
        for y in 0..128u32 {
            for x in 0..128u32 {
                let h =
                    terrain.field.get(x as i64, y as i64) + outcome.delta[(y * 128 + x) as usize];
                assert!((-0.01..=400.01).contains(&h), "({x},{y}) out of range: {h}");
            }
        }
    }

    /// The D12 shot-noise regression tests. On truly flat terrain, uniform
    /// rain must do *nothing* — slope-proportional capacity plus stagnation
    /// death means every droplet dies empty where it spawned. (The old
    /// capacity floor carved ~a pit per droplet here.)
    #[test]
    fn flat_terrain_untouched() {
        let field = TerrainField::flat(128, 128, 1.0, 0.0, 400.0, false, 50.0);
        let terrain = EditableTerrain::new(field);
        let settings = test_settings();
        let outcome = run_job(&terrain, &settings);
        assert!(
            outcome.changed.is_none(),
            "rain on flat ground must be a no-op: {:?}",
            outcome.changed
        );
    }

    /// Around a hill, the plain's texture must be deposition-dominated:
    /// sediment fans run out from the cone, and the only carving allowed is
    /// streams incising through their own deposits — a sliver of the
    /// deposited volume, nothing like the old per-spawn pockmarks.
    #[test]
    fn flat_plain_gains_not_loses() {
        let terrain = mound_terrain();
        let settings = test_settings();
        let outcome = run_job(&terrain, &settings);
        let (mut neg_sum, mut pos_sum) = (0.0f32, 0.0f32);
        for y in 0..128u32 {
            for x in 0..128u32 {
                // Off the cone (base radius 40) with margin for the deposit
                // brush and thermal creep at its foot.
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
            neg_sum < 0.01 * pos_sum,
            "plain carving must be a sliver of deposition: -{neg_sum} vs +{pos_sum}"
        );
    }

    /// The determinism tripwire (D12): same seed, same settings → bit-equal
    /// deltas, guarding the spawn-order batch merge (and, later, the flow
    /// sort tie-break and single-threaded blur) forever.
    #[test]
    fn same_seed_same_result() {
        let terrain = mound_terrain();
        let settings = test_settings();
        let a = run_job(&terrain, &settings);
        let b = run_job(&terrain, &settings);
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
        let settings = test_settings();
        let outcome = run_job(&terrain, &settings);
        assert!(outcome.changed.is_some());
        // Beyond the mask plus thermal creep (one texel per iteration), the
        // terrain must be untouched.
        let safe_x = 60 + settings.thermal_iterations as usize + 2;
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
            droplet_density: 0.0, // thermal only
            ..default()
        };
        let outcome = run_job(&terrain, &settings);
        assert!(outcome.changed.is_some());
        let spike = outcome.delta[32 * 64 + 32];
        let neighbor = outcome.delta[32 * 64 + 33];
        assert!(spike < -1.0, "spike must shed material: {spike}");
        assert!(neighbor > 0.0, "talus must land nearby: {neighbor}");
    }

    #[test]
    fn looping_droplets_cross_the_seam() {
        // A ridge along the -X edge of a looping map: droplets spawned on it
        // flow downhill across the seam without panicking.
        let mut field = TerrainField::flat(64, 64, 1.0, 0.0, 400.0, true, 50.0);
        for y in 0..64 {
            for x in 0..64 {
                // Toroidal distance from the x = 0 column.
                let d = (x as i64).min((-(x as i64)).rem_euclid(64)) as f32;
                field.set(x, y, 50.0 + (100.0 - d * 12.0).max(0.0));
            }
        }
        let terrain = EditableTerrain::new(field);
        let settings = ErosionSettings {
            droplet_density: 1.25,
            ..default()
        };
        let outcome = run_job(&terrain, &settings);
        let changed = outcome.changed.expect("must erode");
        assert!(!changed.is_empty());
        // Both sides of the seam saw movement.
        let left: f32 = (0..64).map(|y| outcome.delta[y * 64].abs()).sum();
        let right: f32 = (0..64).map(|y| outcome.delta[y * 64 + 63].abs()).sum();
        assert!(left > 0.0 && right > 0.0, "seam sides: {left} / {right}");
    }

    #[test]
    fn progress_counts_all_work() {
        let terrain = mound_terrain();
        let settings = ErosionSettings {
            droplet_density: 0.06,
            ..default()
        };
        let mut job = ErosionJob::new(&terrain, &settings);
        job.seed = 7;
        let done = job.done.clone();
        let droplets = job.droplets;
        let total = job.work_total;
        job.run();
        assert_eq!(done.load(Ordering::Relaxed), total);
        // Flow passes are real work: the total must cover more than droplets.
        assert!(total > droplets, "flow passes must count: {total}");
    }

    /// Flow accumulation routes every texel's rain to its steepest-descent
    /// neighbor, high-to-low: a tilted plane accumulates linearly down each
    /// column, and on a looping profile drainage crosses the seam (D6).
    #[test]
    fn flow_map_routes_rain_downhill() {
        // A plane tilted along +Y: row y receives everything above it.
        let mut field = TerrainField::flat(16, 16, 1.0, 0.0, 400.0, false, 0.0);
        for y in 0..16u32 {
            for x in 0..16u32 {
                field.set(x, y, 300.0 - y as f32 * 10.0);
            }
        }
        let terrain = EditableTerrain::new(field);
        let job = ErosionJob::new(&terrain, &ErosionSettings::default());
        let acc = job.flow_accumulation();
        for y in 0..16u32 {
            for x in 0..16u32 {
                assert_eq!(acc[(y * 16 + x) as usize], (y + 1) as f32, "({x},{y})");
            }
        }

        // A toroidal ridge peaking at x = 8: the right-hand chain (x 9..15)
        // keeps descending across the seam into x = 0, which then collects
        // the whole row; a finite map can't wrap, so x = 0 only gets the
        // left-hand chain.
        let profile = |looping: bool| {
            let mut field = TerrainField::flat(16, 16, 1.0, 0.0, 400.0, looping, 0.0);
            for y in 0..16u32 {
                for x in 0..16i64 {
                    let d = (x - 8).abs().min(16 - (x - 8).abs()) as f32;
                    field.set(x as u32, y, 300.0 - d * 10.0);
                }
            }
            let terrain = EditableTerrain::new(field);
            ErosionJob::new(&terrain, &ErosionSettings::default()).flow_accumulation()
        };
        let looped = profile(true);
        let finite = profile(false);
        for y in 0..16usize {
            assert_eq!(
                looped[y * 16],
                16.0,
                "row {y}: drainage must cross the seam"
            );
            assert!(finite[y * 16] < 16.0, "row {y}: a finite map can't wrap");
        }
    }

    /// Deposits must land smooth (D12): with the blur on, off-cone sediment
    /// shows no sharp single-texel prominence, and it's strictly smoother
    /// than the same run without the blur.
    #[test]
    fn deposits_are_smooth() {
        let terrain = mound_terrain();
        let prominence = |blur: u32| {
            let settings = ErosionSettings {
                deposit_blur_radius: blur,
                keep_maps: true,
                ..test_settings()
            };
            let outcome = run_job(&terrain, &settings);
            let maps = outcome.maps.expect("keep_maps retains analysis maps");
            let at = |x: i64, y: i64| maps.deposit[(y * 128 + x) as usize];
            let mut worst = 0.0f32;
            for y in 1..127i64 {
                for x in 1..127i64 {
                    let d = Vec2::new(x as f32 - 64.0, y as f32 - 64.0).length();
                    if d <= 55.0 {
                        continue;
                    }
                    let mut sum = 0.0;
                    for (dx, dy, _) in NEIGHBORS {
                        sum += at(x + dx as i64, y + dy as i64);
                    }
                    worst = worst.max(at(x, y) - sum / 8.0);
                }
            }
            worst
        };
        let blurred = prominence(2);
        let raw = prominence(0);
        // Measured ~0.57 m vs ~5.5 m raw at this (8× default) density — the
        // blur buys roughly an order of magnitude; assert 4× for headroom.
        assert!(blurred < 1.0, "blurred deposit prominence: {blurred} m");
        assert!(
            blurred < raw / 4.0,
            "blur must smooth substantially: {blurred} vs {raw}"
        );
    }

    /// The flow boost must concentrate carving: with it on, the most-eroded
    /// texels hold a strictly larger share of the total eroded volume —
    /// connected channels instead of diffuse scratches.
    #[test]
    fn flow_concentrates_erosion() {
        let terrain = mound_terrain();
        let share = |flow_strength: f32| {
            let settings = ErosionSettings {
                flow_strength,
                ..test_settings()
            };
            let outcome = run_job(&terrain, &settings);
            let mut eroded: Vec<f32> = outcome.delta.iter().copied().filter(|&d| d < 0.0).collect();
            eroded.sort_unstable_by(|a, b| a.total_cmp(b)); // most negative first
            let total: f32 = eroded.iter().sum();
            let top: f32 = eroded[..eroded.len() / 20].iter().sum();
            top / total
        };
        let diffuse = share(0.0);
        let focused = share(6.0);
        assert!(
            focused > diffuse,
            "flow must concentrate carving: top-5% share {focused} vs {diffuse}"
        );
    }
}
