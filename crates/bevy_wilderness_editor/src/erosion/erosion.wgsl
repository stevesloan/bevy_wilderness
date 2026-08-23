// Pipe-model ("virtual pipes") hydraulic erosion — Mei, Decaudin & Hu 2007 —
// plus a proportional thermal (talus) pass, run as compute kernels over the
// erosion domain (the padded mask bbox, or the whole map). One simulation
// iteration is the dispatch sequence
//
//   flux → water → erode → advect → thermal
//
// with `settle` once after the last iteration. All kernels write only their
// own texel, so a run is deterministic on a given device (no atomics, no
// scattered writes — the GPU twin of the old CPU sim's §10 rules).
//
// Heights, water and sediment are all in *meters* (sediment as an equivalent
// height of a full column); velocities in m/s. The ping-pong pairs
// (height_in/out for the thermal pass, sed_in/out for advection) swap roles
// every iteration via alternating bind groups — `thermal` runs even at
// rate 0 so the parity of both pairs stays in lockstep.
//
// ⚠️ Buffers are sized for wgpu's *default* limits: nothing here may exceed
// 128 MiB per binding at 4096² (so flux and the analysis accumulators are
// split into vec2/f32 buffers, never vec4), and no pipeline layout may name
// more than 8 storage buffers (so passes bind per-family subsets of the
// module's bindings: flow {1,3,4,5,6,9}, sediment {1,3,6,7,8,9,10,11},
// thermal {1,2,9}).

struct Params {
    // Domain size in texels.
    dims: vec2<i32>,
    // 1 = toroidal wrap (the domain is a full looping map); else edges clamp
    // for height reads and are open (water drains off) for flow.
    wrap: u32,
    // 1 = the mask buffer is real; 0 = unmasked (weight 1 everywhere).
    masked: u32,
    // Simulation timestep, seconds.
    dt: f32,
    // Meters per texel.
    cell: f32,
    // Rainfall, m/s, scaled by the mask weight.
    rain: f32,
    // Water lost per second (fraction).
    evaporation: f32,
    // Kc: sediment capacity per unit of tilt × speed.
    capacity: f32,
    // Ks: fraction of the capacity surplus dissolved per second.
    dissolve: f32,
    // Kd: fraction of the sediment surplus deposited per second.
    deposition: f32,
    // Floor on sin(tilt) in the capacity term, so channels that have graded
    // themselves flat keep incising instead of stalling.
    min_tilt: f32,
    // Water deeper than this stops eroding (deep pools armor the bed);
    // 0 disables the fade.
    max_depth: f32,
    // tan(talus angle): the height step per meter the thermal pass relaxes to.
    talus: f32,
    // Fraction of the talus excess relaxed per second.
    thermal: f32,
    // 1 = accumulate wear/deposit/discharge into the accum buffers.
    keep_maps: u32,
    // Velocity clamp, m/s — 0.9 × cell/dt, the semi-Lagrangian CFL bound.
    // Purely numerical; the *physical* speed cap is max_speed below.
    vmax: f32,
    // Cap on the flow speed entering the capacity term, m/s. Without it,
    // near-dry films (speed = flux / depth) hit the CFL clamp — tens of
    // m/s — and carve washboard stripes wherever the water is thinnest.
    max_speed: f32,
}

// Pipe friction, fraction of flux lost per second: damps the standing-wave
// ringing (ridges perpendicular to flow) the frictionless model builds up.
const FRICTION: f32 = 0.15;

// Water shallower than this (meters) fades erosion in from zero — a film has
// to actually gather before it may carve, which kills the thin-film striping.
const MIN_ERODE_DEPTH: f32 = 0.005;

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read_write> height_in: array<f32>;
@group(0) @binding(2) var<storage, read_write> height_out: array<f32>;
@group(0) @binding(3) var<storage, read_write> water: array<f32>;
// Outflow flux, m³/s: flux_lr.x toward -X, .y toward +X; flux_tb likewise -Y/+Y.
@group(0) @binding(4) var<storage, read_write> flux_lr: array<vec2<f32>>;
@group(0) @binding(5) var<storage, read_write> flux_tb: array<vec2<f32>>;
@group(0) @binding(6) var<storage, read_write> vel: array<vec2<f32>>;
@group(0) @binding(7) var<storage, read_write> sed_in: array<f32>;
@group(0) @binding(8) var<storage, read_write> sed_out: array<f32>;
@group(0) @binding(9) var<storage, read> mask: array<f32>;
// Analysis accumulators (dummy 16-byte buffers unless keep_maps):
// wear/deposit in meters, and discharge ∫|v|·depth·dt — the pipe model's
// "flow accumulation".
@group(0) @binding(10) var<storage, read_write> accum_wd: array<vec2<f32>>;
@group(0) @binding(11) var<storage, read_write> accum_flow: array<f32>;

const G: f32 = 9.81;

// Storage index for possibly-out-of-range texel coordinates: wraps when
// looping, -1 beyond a finite edge (the read/write is dropped — the same
// convention as the CPU field's wrap_texel).
fn index_of(p: vec2<i32>) -> i32 {
    var q = p;
    if P.wrap == 1u {
        q = (q % P.dims + P.dims) % P.dims;
    } else if q.x < 0 || q.y < 0 || q.x >= P.dims.x || q.y >= P.dims.y {
        return -1;
    }
    return q.y * P.dims.x + q.x;
}

// Always-valid index: wraps when looping, edge-clamps otherwise (the CPU
// field's `get` semantics) — for height gradients at the border.
fn index_clamped(p: vec2<i32>) -> i32 {
    var q = p;
    if P.wrap == 1u {
        q = (q % P.dims + P.dims) % P.dims;
    } else {
        q = clamp(q, vec2<i32>(0), P.dims - 1);
    }
    return q.y * P.dims.x + q.x;
}

fn mask_at(i: i32) -> f32 {
    if P.masked == 1u {
        return mask[i];
    }
    return 1.0;
}

// Total hydraulic head (ground + water) seen from a cell toward `p`; when `p`
// is off a finite domain, the boundary is open: same ground level, no water,
// so exactly the cell's own water depth drains over the edge.
fn head_toward(p: vec2<i32>, own_ground: f32) -> f32 {
    let j = index_of(p);
    if j < 0 {
        return own_ground;
    }
    return height_in[j] + water[j];
}

// ---------------------------------------------------------------- flux pass

@compute @workgroup_size(8, 8)
fn flux_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if p.x >= P.dims.x || p.y >= P.dims.y {
        return;
    }
    let i = p.y * P.dims.x + p.x;
    let b = height_in[i];
    let d = water[i];
    let head = b + d;
    let friction = max(0.0, 1.0 - FRICTION * P.dt);
    var lr = flux_lr[i] * friction;
    var tb = flux_tb[i] * friction;
    // f += dt · g · (pipe area / pipe length) · Δhead, with area = cell² and
    // length = cell (the standard choice), never negative per direction.
    let k = P.dt * G * P.cell;
    lr.x = max(0.0, lr.x + k * (head - head_toward(p + vec2<i32>(-1, 0), b)));
    lr.y = max(0.0, lr.y + k * (head - head_toward(p + vec2<i32>(1, 0), b)));
    tb.x = max(0.0, tb.x + k * (head - head_toward(p + vec2<i32>(0, -1), b)));
    tb.y = max(0.0, tb.y + k * (head - head_toward(p + vec2<i32>(0, 1), b)));
    // Scale so a step never drains more volume than the column holds.
    let total = lr.x + lr.y + tb.x + tb.y;
    if total > 0.0 {
        let scale = min(1.0, d * P.cell * P.cell / (total * P.dt));
        lr = lr * scale;
        tb = tb * scale;
    }
    flux_lr[i] = lr;
    flux_tb[i] = tb;
}

// --------------------------------------------------------------- water pass

fn flux_lr_of(p: vec2<i32>) -> vec2<f32> {
    let j = index_of(p);
    if j < 0 {
        return vec2<f32>(0.0);
    }
    return flux_lr[j];
}

fn flux_tb_of(p: vec2<i32>) -> vec2<f32> {
    let j = index_of(p);
    if j < 0 {
        return vec2<f32>(0.0);
    }
    return flux_tb[j];
}

@compute @workgroup_size(8, 8)
fn water_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if p.x >= P.dims.x || p.y >= P.dims.y {
        return;
    }
    let i = p.y * P.dims.x + p.x;
    let lr = flux_lr[i];
    let tb = flux_tb[i];
    let fl = flux_lr_of(p + vec2<i32>(-1, 0));
    let fr = flux_lr_of(p + vec2<i32>(1, 0));
    let ft = flux_tb_of(p + vec2<i32>(0, -1));
    let fb = flux_tb_of(p + vec2<i32>(0, 1));
    let inflow = fl.y + fr.x + ft.y + fb.x;
    let outflow = lr.x + lr.y + tb.x + tb.y;
    let d1 = water[i];
    var d2 = max(0.0, d1 + P.dt * (inflow - outflow) / (P.cell * P.cell));
    // Mean per-axis throughflow → depth-averaged velocity.
    let fx = 0.5 * (fl.y - lr.x + lr.y - fr.x);
    let fy = 0.5 * (ft.y - tb.x + tb.y - fb.x);
    let dbar = 0.5 * (d1 + d2);
    var v = vec2<f32>(0.0);
    if dbar > 1e-4 {
        v = vec2<f32>(fx, fy) / (dbar * P.cell);
        let speed = length(v);
        if speed > P.vmax {
            v = v * (P.vmax / speed);
        }
    }
    // Rain lands (mask-weighted — rain only ever falls on the mask, like the
    // old droplet spawns), then evaporation takes its cut. The ±25% hash
    // jitter is deterministic per texel; it decorrelates the D4 lattice's
    // taste for perfectly straight, evenly spaced rills.
    d2 = d2 + P.dt * P.rain * rain_jitter(p) * mask_at(i);
    d2 = d2 * max(0.0, 1.0 - P.evaporation * P.dt);
    water[i] = d2;
    vel[i] = v;
}

// Deterministic per-texel rain weight in 0.75..1.25 (lowbias32 hash).
fn rain_jitter(p: vec2<i32>) -> f32 {
    var n = u32(p.x) ^ (u32(p.y) << 16u) ^ (u32(p.y) >> 16u);
    n = n ^ (n >> 16u);
    n = n * 0x7feb352du;
    n = n ^ (n >> 15u);
    n = n * 0x846ca68bu;
    n = n ^ (n >> 16u);
    return 0.75 + 0.5 * (f32(n & 0xffffffu) / 16777216.0);
}

// --------------------------------------------------- erosion-deposition pass

@compute @workgroup_size(8, 8)
fn erode_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if p.x >= P.dims.x || p.y >= P.dims.y {
        return;
    }
    let i = p.y * P.dims.x + p.x;
    // Ground tilt from a Sobel gradient (edge-clamped like the CPU field):
    // the 3×3 support smooths the slope estimate, so a single-texel dip
    // can't self-amplify into the anti-dune ripple feedback.
    let h = array<f32, 9>(
        height_in[index_clamped(p + vec2<i32>(-1, -1))],
        height_in[index_clamped(p + vec2<i32>(0, -1))],
        height_in[index_clamped(p + vec2<i32>(1, -1))],
        height_in[index_clamped(p + vec2<i32>(-1, 0))],
        height_in[i],
        height_in[index_clamped(p + vec2<i32>(1, 0))],
        height_in[index_clamped(p + vec2<i32>(-1, 1))],
        height_in[index_clamped(p + vec2<i32>(0, 1))],
        height_in[index_clamped(p + vec2<i32>(1, 1))],
    );
    let dbdx = ((h[2] + 2.0 * h[5] + h[8]) - (h[0] + 2.0 * h[3] + h[6])) / (8.0 * P.cell);
    let dbdy = ((h[6] + 2.0 * h[7] + h[8]) - (h[0] + 2.0 * h[1] + h[2])) / (8.0 * P.cell);
    let grad2 = dbdx * dbdx + dbdy * dbdy;
    let tilt = max(sqrt(grad2 / (1.0 + grad2)), P.min_tilt);
    let d = water[i];
    // The capacity term uses *physically* capped speed (max_speed), not the
    // CFL clamp — near-dry films otherwise report absurd speeds and stripe.
    let speed = min(length(vel[i]), P.max_speed);
    // Deep standing water armors the bed (fade out toward max_depth); a
    // too-thin film hasn't gathered enough to carve (fade in from zero).
    var depth_fade = clamp(d / MIN_ERODE_DEPTH, 0.0, 1.0);
    if P.max_depth > 0.0 {
        depth_fade = depth_fade * clamp(1.0 - d / P.max_depth, 0.0, 1.0);
    }
    let c = P.capacity * tilt * speed * depth_fade;
    let s = sed_in[i];
    let w = mask_at(i);
    var b = height_in[i];
    var s_new = s;
    if c > s {
        // Under capacity: dissolve ground into the flow. The mask weights the
        // amount itself (not just the ground change), so unmasked ground
        // neither carves nor donates phantom sediment.
        let amt = P.dt * P.dissolve * (c - s) * w;
        b = b - amt;
        s_new = s + amt;
        if P.keep_maps == 1u {
            accum_wd[i].x = accum_wd[i].x + amt;
        }
    } else {
        // Over capacity: settle the surplus out.
        let amt = min(P.dt * P.deposition * (s - c), s) * w;
        b = b + amt;
        s_new = s - amt;
        if P.keep_maps == 1u {
            accum_wd[i].y = accum_wd[i].y + amt;
        }
    }
    height_in[i] = b;
    sed_in[i] = s_new;
    if P.keep_maps == 1u {
        accum_flow[i] = accum_flow[i] + speed * d * P.dt;
    }
}

// ---------------------------------------------- sediment advection (semi-L)

@compute @workgroup_size(8, 8)
fn advect_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if p.x >= P.dims.x || p.y >= P.dims.y {
        return;
    }
    let i = p.y * P.dims.x + p.x;
    // Back-trace along the flow; sample the sediment field bilinearly there.
    // Off-domain taps contribute zero — sediment carried over a finite edge
    // leaves, exactly like a droplet flowing off the map.
    let q = vec2<f32>(f32(p.x), f32(p.y)) - vel[i] * (P.dt / P.cell);
    let q0 = floor(q);
    let fr = q - q0;
    let c = vec2<i32>(q0);
    var s = 0.0;
    let j00 = index_of(c);
    let j10 = index_of(c + vec2<i32>(1, 0));
    let j01 = index_of(c + vec2<i32>(0, 1));
    let j11 = index_of(c + vec2<i32>(1, 1));
    if j00 >= 0 { s = s + sed_in[j00] * (1.0 - fr.x) * (1.0 - fr.y); }
    if j10 >= 0 { s = s + sed_in[j10] * fr.x * (1.0 - fr.y); }
    if j01 >= 0 { s = s + sed_in[j01] * (1.0 - fr.x) * fr.y; }
    if j11 >= 0 { s = s + sed_in[j11] * fr.x * fr.y; }
    sed_out[i] = s;
}

// ------------------------------------------------------------- thermal pass

// Material `q` (at storage index `i_q`) sheds this iteration, distributed
// over its below-talus neighbors proportionally to their excess. `toward` < 0
// returns the total outflow; otherwise the share sent to that storage index.
// Donor and receiver recompute this identically — the gather formulation that
// keeps the pass single-writer and deterministic.
fn thermal_flow(q: vec2<i32>, i_q: i32, toward: i32) -> f32 {
    var offs = array<vec2<i32>, 8>(
        vec2<i32>(-1, -1), vec2<i32>(0, -1), vec2<i32>(1, -1),
        vec2<i32>(-1, 0), vec2<i32>(1, 0),
        vec2<i32>(-1, 1), vec2<i32>(0, 1), vec2<i32>(1, 1),
    );
    var dist = array<f32, 8>(
        1.41421356, 1.0, 1.41421356, 1.0, 1.0, 1.41421356, 1.0, 1.41421356,
    );
    let bq = height_in[i_q];
    var sum = 0.0;
    var max_e = 0.0;
    var e_toward = 0.0;
    for (var n = 0u; n < 8u; n = n + 1u) {
        let j = index_of(q + offs[n]);
        if j < 0 {
            continue;
        }
        let e = bq - height_in[j] - P.talus * P.cell * dist[n];
        if e > 0.0 {
            sum = sum + e;
            max_e = max(max_e, e);
            if j == toward {
                e_toward = e_toward + e;
            }
        }
    }
    if sum <= 0.0 {
        return 0.0;
    }
    // Half the largest excess would level that pair to the talus angle; the
    // rate (clamped for stability) is how much of it moves per iteration.
    let total = min(P.thermal * P.dt, 0.5) * 0.5 * max_e * mask_at(i_q);
    if toward < 0 {
        return total;
    }
    return total * e_toward / sum;
}

@compute @workgroup_size(8, 8)
fn thermal_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if p.x >= P.dims.x || p.y >= P.dims.y {
        return;
    }
    let i = p.y * P.dims.x + p.x;
    var b = height_in[i] - thermal_flow(p, i, -1);
    var offs = array<vec2<i32>, 8>(
        vec2<i32>(-1, -1), vec2<i32>(0, -1), vec2<i32>(1, -1),
        vec2<i32>(-1, 0), vec2<i32>(1, 0),
        vec2<i32>(-1, 1), vec2<i32>(0, 1), vec2<i32>(1, 1),
    );
    for (var n = 0u; n < 8u; n = n + 1u) {
        let np = p + offs[n];
        let j = index_of(np);
        if j < 0 {
            continue;
        }
        b = b + thermal_flow(np, j, i);
    }
    height_out[i] = b;
}

// -------------------------------------------------------------- settle pass

// After the last iteration: whatever is still suspended settles where it
// stands (mask-weighted), like a dying droplet leaving its load.
@compute @workgroup_size(8, 8)
fn settle_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if p.x >= P.dims.x || p.y >= P.dims.y {
        return;
    }
    let i = p.y * P.dims.x + p.x;
    let amt = sed_in[i] * mask_at(i);
    height_in[i] = height_in[i] + amt;
    sed_in[i] = 0.0;
    if P.keep_maps == 1u {
        accum_wd[i].y = accum_wd[i].y + amt;
    }
}
