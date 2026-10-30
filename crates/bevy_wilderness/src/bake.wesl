// Bakes the terrain splat material into a texture indexed by world XZ. Runs the
// same splat blend as the main pass but writes raw channels unlit, so the main
// pass can sample the result instead of blending per-fragment. Rendered by a
// top-down orthographic camera over a flat quad covering the terrain.
import bevy_pbr::render::forward_io::VertexOutput;

@group(constants::MATERIAL_BIND_GROUP) @binding(0) var heightmap_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var heightmap_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(2) var<uniform> texel_size: f32;
@group(constants::MATERIAL_BIND_GROUP) @binding(3) var<uniform> minmax: vec2<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(4) var albedo_array: texture_2d_array<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(5) var albedo_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(8) var<uniform> params: TerrainParams;
@group(constants::MATERIAL_BIND_GROUP) @binding(9) var normal_array: texture_2d_array<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(10) var normal_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(11) var orm_array: texture_2d_array<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(12) var orm_sampler: sampler;
// 0 = albedo target (rgb albedo, a cavity); 1 = normal target (rg octahedral
// world normal, b roughness, a = packed top-2 material ids + blend); 3 = sun
// shadow-ceiling field (see `shadow_ceiling`).
@group(constants::MATERIAL_BIND_GROUP) @binding(13) var<uniform> output_mode: u32;
// Normalized direction toward the fixed sun.
@group(constants::MATERIAL_BIND_GROUP) @binding(14) var<uniform> sun_direction: vec3<f32>;
// 1 = looping terrain: read the heightmap toroidally so the shadow march and
// the cavity ring wrap across tile edges; 0 = finite (clamp).
@group(constants::MATERIAL_BIND_GROUP) @binding(15) var<uniform> looping: u32;

struct TerrainParams {
    tiling_scale: vec4<f32>,
    height_blend: vec4<f32>,
    roughness: vec4<f32>,
    normal_strength: vec4<f32>,
    slope_min: vec4<f32>,
    slope_max: vec4<f32>,
    slope_blend: vec4<f32>,
    height_min: vec4<f32>,
    height_max: vec4<f32>,
    height_range_blend: vec4<f32>,
    cavity_min: vec4<f32>,
    cavity_max: vec4<f32>,
    cavity_blend: vec4<f32>,
    layer_count: u32,
}

// Weight of a band: 1 inside [lo, hi], ramping to 0 over `blend` just outside
// each edge. `lo` at/below the input's min (or `hi` at/above its max) makes that
// side open-ended (weight stays 1 there).
fn band(x: f32, lo: f32, hi: f32, blend: f32) -> f32 {
    let up = smoothstep(lo - blend, lo, x);
    let down = 1.0 - smoothstep(hi, hi + blend, x);
    return up * down;
}

const MAX_LAYERS: u32 = 4u;

struct SplatResult {
    color: vec3<f32>,
    normal: vec3<f32>,
    roughness: f32,
    // Top-2 layer ids + blend, packed into 8 bits (2+2+4) — see `splat_terrain`.
    // Stored in the RVT normal target's alpha (the metallic slot, unused by
    // terrain); the main pass unpacks it to blend per-material detail.
    material_id: f32,
}

fn heightmap_uv(world_xz: vec2<f32>) -> vec2<f32> {
    let texture_size = vec2<f32>(textureDimensions(heightmap_texture));
    return world_xz / (texture_size * texel_size) + 0.5;
}

// Fetch one heightmap texel, wrapping toroidally when looping so the shadow
// march and cavity ring read the tiled heightmap; otherwise clamp to the edge.
fn load_height_texel(p: vec2<i32>, size: vec2<i32>, lod: i32) -> f32 {
    var idx = clamp(p, vec2(0), size - 1);
    if looping != 0u {
        idx = ((p % size) + size) % size;
    }
    return textureLoad(heightmap_texture, idx, lod).r;
}

fn height_bilinear(uv: vec2<f32>, lod: i32) -> f32 {
    let tex_size = vec2<i32>(textureDimensions(heightmap_texture, lod));
    let pos = uv * vec2<f32>(tex_size);
    let p0 = vec2<i32>(floor(pos));
    let f = pos - floor(pos);
    let h00 = load_height_texel(p0, tex_size, lod);
    let h10 = load_height_texel(p0 + vec2(1, 0), tex_size, lod);
    let h01 = load_height_texel(p0 + vec2(0, 1), tex_size, lod);
    let h11 = load_height_texel(p0 + vec2(1, 1), tex_size, lod);
    let hx0 = mix(h00, h10, f.x);
    let hx1 = mix(h01, h11, f.x);
    return mix(hx0, hx1, f.y);
}

fn geo_normal(world_xz: vec2<f32>) -> vec3<f32> {
    let texture_size = vec2<f32>(textureDimensions(heightmap_texture));
    let uv = world_xz / (texture_size * texel_size) + 0.5;
    let step = 1.0 / texture_size;
    // Sample through height_bilinear (not the sampler) so the slope wraps with
    // the rest of the toroidal reads when looping — no seam in the baked normal.
    let h_r = height_bilinear(uv + vec2(step.x, 0.0), 0);
    let h_l = height_bilinear(uv - vec2(step.x, 0.0), 0);
    let h_t = height_bilinear(uv + vec2(0.0, step.y), 0);
    let h_b = height_bilinear(uv - vec2(0.0, step.y), 0);
    let scale = (minmax.y - minmax.x) / (2.0 * texel_size);
    let dh_dx = (h_r - h_l) * scale;
    let dh_dy = (h_t - h_b) * scale;
    return normalize(vec3(-dh_dx, 1.0, -dh_dy));
}

// World-space terrain height at a position.
fn terrain_height(world_xz: vec2<f32>) -> f32 {
    let h = height_bilinear(heightmap_uv(world_xz), 0);
    return h * (minmax.y - minmax.x) + minmax.x;
}

// Terrain curvature at `world_xz` (height `origin_h`). Drives the splat's
// cavity bands and is stored in the albedo target's alpha.
fn cavity_at(world_xz: vec2<f32>, origin_h: f32) -> f32 {
    // Cavity: concave (gullies, hollows) vs convex (ridges, knolls). <0.5
    // concave, >0.5 convex, 0.5 flat. Measured as the *depression angle* — how
    // far the ring of terrain at radius r sits above (concave) or below
    // (convex) this point, over r — so it is independent of heightmap
    // resolution and height range. On natural terrain the median is ~3° and
    // the 95th percentile ~12°, so ±15° spans the useful range with only a few
    // percent saturating. (A Laplacian with a fixed gain saturated ~85% of the
    // example map to pure 0 / 1.)
    const CAVITY_RING_TAPS = 8;
    const CAVITY_SATURATION = 0.2679;  // tan(15°)
    let r = max(2.0 * texel_size, 16.0);
    var ring = 0.0;
    for (var k = 0; k < CAVITY_RING_TAPS; k++) {
        let a = f32(k) * (6.2831853 / f32(CAVITY_RING_TAPS));
        ring += terrain_height(world_xz + vec2<f32>(cos(a), sin(a)) * r);
    }
    let depression = (ring / f32(CAVITY_RING_TAPS) - origin_h) / r;
    return clamp(0.5 - 0.5 * depression / CAVITY_SATURATION, 0.0, 1.0);
}

// --- Stochastic hex tiling (Mikkelsen) ---
// Samples a tiled texture 3× on a randomized hex lattice and blends, hiding the
// repetition. Only run in the bake (one-time), never per-frame — it needs 3×
// samples and `textureSampleGrad` (per-cell offsets break implicit derivatives).

fn hex_hash(p: vec2<f32>) -> vec2<f32> {
    let r = vec2<f32>(dot(p, vec2<f32>(127.1, 311.7)), dot(p, vec2<f32>(269.5, 183.3)));
    return fract(sin(r) * 43758.5453);
}

// Skewed triangle (hex) grid: barycentric weights + the 3 cell vertices for `uv`.
fn triangle_grid(
    uv: vec2<f32>,
    w: ptr<function, vec3<f32>>,
    v1: ptr<function, vec2<f32>>,
    v2: ptr<function, vec2<f32>>,
    v3: ptr<function, vec2<f32>>,
) {
    // Cells span ~1 texture repeat: each cell is a differently-offset crop.
    // Smaller cells speckle under minification; larger ones show repetition.
    let p = uv;
    let skewed = vec2<f32>(p.x - 0.57735027 * p.y, 1.15470054 * p.y);
    let base = floor(skewed);
    let f = fract(skewed);
    let fz = 1.0 - f.x - f.y;
    let s = step(0.0, -fz);
    let s2 = 2.0 * s - 1.0;
    *w = vec3<f32>(-fz * s2, s - f.y * s2, s - f.x * s2);
    *v1 = base + vec2<f32>(s, s);
    *v2 = base + vec2<f32>(s, 1.0 - s);
    *v3 = base + vec2<f32>(1.0 - s, s);
}

fn hex_sample(
    tex: texture_2d_array<f32>,
    samp: sampler,
    layer: u32,
    uv: vec2<f32>,
    ddx: vec2<f32>,
    ddy: vec2<f32>,
) -> vec4<f32> {
    var w: vec3<f32>;
    var v1: vec2<f32>;
    var v2: vec2<f32>;
    var v3: vec2<f32>;
    triangle_grid(uv, &w, &v1, &v2, &v3);
    let c1 = textureSampleGrad(tex, samp, uv + hex_hash(v1), layer, ddx, ddy);
    let c2 = textureSampleGrad(tex, samp, uv + hex_hash(v2), layer, ddx, ddy);
    let c3 = textureSampleGrad(tex, samp, uv + hex_hash(v3), layer, ddx, ddy);
    // Sharpen the barycentric weights to keep transitions crisp (avoid ghosting).
    let ws = pow(w, vec3<f32>(7.0)) + vec3<f32>(1e-6);
    let wn = ws / (ws.x + ws.y + ws.z);
    return c1 * wn.x + c2 * wn.y + c3 * wn.z;
}

// Blends the terrain layers at one world position and returns the packed RVT
// channels. This is the *only* place the splat runs — the main pass samples the
// baked result. Procedural placement + hex de-tiling.
fn splat_terrain(world_xz: vec2<f32>, normal: vec3<f32>) -> SplatResult {
    // Procedural placement: each layer's weight is the overlap of its slope band
    // (grass→dirt→rock) and its world-height band (e.g. snow above a snowline).
    let slope = acos(clamp(normal.y, -1.0, 1.0));
    let height = terrain_height(world_xz);
    let cavity = cavity_at(world_xz, height);

    var w = array<f32, 4>(0.0, 0.0, 0.0, 0.0);
    for (var i = 0u; i < MAX_LAYERS; i++) {
        if i >= params.layer_count {
            continue;
        }
        w[i] = band(slope, params.slope_min[i], params.slope_max[i], params.slope_blend[i])
             * band(height, params.height_min[i], params.height_max[i], params.height_range_blend[i])
             * band(cavity, params.cavity_min[i], params.cavity_max[i], params.cavity_blend[i]);
    }

    let wsum = w[0] + w[1] + w[2] + w[3];
    if wsum < 1e-4 {
        w[0] = 1.0;
    } else {
        for (var i = 0u; i < MAX_LAYERS; i++) {
            w[i] = w[i] / wsum;
        }
    }

    var colors = array<vec3<f32>, 4>();
    var normals = array<vec3<f32>, 4>();
    var orms = array<vec3<f32>, 4>();
    var scores = array<f32, 4>();
    var maxs = -1e9;
    for (var i = 0u; i < MAX_LAYERS; i++) {
        let tile_uv = world_xz / params.tiling_scale[i];
        let ddx = dpdx(tile_uv);
        let ddy = dpdy(tile_uv);
        let a = hex_sample(albedo_array, albedo_sampler, i, tile_uv, ddx, ddy);
        // Flip X: the reorientation tangent runs -X vs the +X tiling UV, so the
        // normal's red axis is mirrored — concave features (cracks) light up as
        // convex (bumps/veins) without this.
        let n = (hex_sample(normal_array, normal_sampler, i, tile_uv, ddx, ddy).xyz * 2.0 - 1.0)
            * vec3<f32>(-1.0, 1.0, 1.0);
        colors[i] = a.rgb;
        normals[i] = vec3<f32>(n.xy * params.normal_strength[i], n.z);
        orms[i] = hex_sample(orm_array, orm_sampler, i, tile_uv, ddx, ddy).rgb;
        let mask = select(0.0, 1.0, w[i] > 1e-4);
        scores[i] = (w[i] + a.a * params.height_blend[i]) * mask - (1.0 - mask) * 1e9;
        maxs = max(maxs, scores[i]);
    }

    const TRANSITION = 0.2;
    var rgb = vec3<f32>(0.0);
    var tn = vec3<f32>(0.0);
    var orm = vec3<f32>(0.0);
    var rough = 0.0;
    var bsum = 0.0;
    // Track the top two contributing layers (b0 >= b1) so the detail overlay can
    // lerp their detail normals instead of snapping at material boundaries.
    var b0 = -1.0;
    var i0 = 0u;
    var b1 = -1.0;
    var i1 = 0u;
    for (var i = 0u; i < MAX_LAYERS; i++) {
        let b = max(0.0, scores[i] - (maxs - TRANSITION));
        rgb += colors[i] * b;
        tn += normals[i] * b;
        orm += orms[i] * b;
        rough += params.roughness[i] * b;
        bsum += b;
        if b > b0 {
            b1 = b0; i1 = i0;
            b0 = b; i0 = i;
        } else if b > b1 {
            b1 = b; i1 = i;
        }
    }
    let inv = 1.0 / max(bsum, 1e-4);
    rgb *= inv;
    orm *= inv;
    tn = normalize(tn);

    let ref_axis = select(vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(1.0, 0.0, 0.0), abs(normal.z) > 0.99);
    let tangent = normalize(cross(ref_axis, normal));
    let bitangent = cross(normal, tangent);

    var out: SplatResult;
    out.color = rgb;
    out.normal = normalize(tangent * tn.x + bitangent * tn.y + normal * tn.z);
    out.roughness = (rough * inv) * orm.g;
    // Pack the two dominant layer indices + their blend into 8 bits (2+2+4). The
    // blend `t2` (0..1) maps to a detail-normal lerp of 0..0.5 in the main pass.
    let t2 = clamp(2.0 * b1 / max(b0 + b1, 1e-4), 0.0, 1.0);
    let q = floor(t2 * 15.0 + 0.5);
    out.material_id = (f32(i0) + f32(i1) * 4.0 + q * 16.0) / 255.0;
    return out;
}

// The sun shadow ceiling: the world height at which the sun ray grazing this
// column's last occluder passes. A point (x, y, z) is lit exactly when
// `y >= ceiling(x, z)` — occlusion by a heightfield under a fixed sun is a 2D
// field with no loss of accuracy, which is what lets a flying character resolve
// its shadowing with one texture fetch instead of a march.
//
// Marching horizontally by `t` along the sun's ground track, an occluder of
// height `h` there shadows everything below `h - t·tan(elevation)`, so the
// ceiling is the max of that over the ray. `.y` also returns the `t` at which
// the max occurred: the distance to the occluder that actually casts here, which
// is what sets penumbra width (the sun is a disc, not a point).
//
// Returns `vec2(ceiling, occluder_distance)`.
fn shadow_ceiling(world_xz: vec2<f32>) -> vec2<f32> {
    // Geometric growth reaches 6 km in ~210 steps. Each step must stay short
    // enough not to jump an occluder: at 1.12 (~50 steps) a ridge between two
    // samples went unseen and the ceiling came out low in ~10% of columns,
    // leaking sun into the ground's shadow near its edges and onto meshes in
    // it. Checked against a 1 m march on a 4096² heightmap: 1.02 leaves <0.1%.
    const STEPS = 256;
    const MAX_DIST = 6000.0;
    const GROWTH = 1.02;
    let STEP0 = texel_size * 1.0;

    // The column's own ground is the floor of the ceiling: at t = 0 the terrain
    // occludes everything below itself. Keeps the result >= `minmax.x`, which the
    // R-channel bias downstream relies on.
    let ground = terrain_height(world_xz);
    var ceiling = ground;
    var distance = 0.0;

    // A sun directly overhead has no ground track to march: nothing but the
    // column's own ground can occlude it.
    let ground_track = sun_direction.xz;
    let track_len = length(ground_track);
    if track_len < 1e-4 {
        return vec2<f32>(ceiling, distance);
    }
    let dir = ground_track / track_len;
    // Height the ray gains per metre travelled horizontally.
    let rise = sun_direction.y / track_len;

    var step = STEP0;
    var t = STEP0;
    for (var i = 0; i < STEPS; i++) {
        if t > MAX_DIST {
            break;
        }
        // Nothing beyond here can win: even terrain at max height would graze
        // below the ceiling we already have. Ends the march early under a high
        // sun, where the ray clears the terrain almost immediately.
        if minmax.y - t * rise <= ceiling {
            break;
        }
        let candidate = terrain_height(world_xz + dir * t) - t * rise;
        if candidate > ceiling {
            ceiling = candidate;
            distance = t;
        }
        step *= GROWTH;
        t += step;
    }
    return vec2<f32>(ceiling, distance);
}

// Octahedral encode of a unit vector into 0..1 (2 channels).
fn oct_wrap(v: vec2<f32>) -> vec2<f32> {
    return (1.0 - abs(v.yx)) * select(vec2(-1.0), vec2(1.0), v >= vec2(0.0));
}

fn oct_encode(n: vec3<f32>) -> vec2<f32> {
    let m = abs(n.x) + abs(n.y) + abs(n.z);
    var v = n.xy / m;
    v = select(oct_wrap(v), v, n.z >= 0.0);
    return v * 0.5 + 0.5;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let world_xz = in.world_position.xz;
    // The shadow field skips the splat entirely — only the heightfield.
    if output_mode == 3u {
        let shadow = shadow_ceiling(world_xz);
        // R is biased by `minmax.x - 1` so it is always >= 1: the bake's
        // readiness sentinel treats an all-zero readback as "not drawn yet", and
        // a legitimately zero height would read as a stall forever.
        return vec4<f32>(shadow.x - (minmax.x - 1.0), shadow.y, 0.0, 1.0);
    }
    let splat = splat_terrain(world_xz, geo_normal(world_xz));
    if output_mode == 0u {
        return vec4<f32>(splat.color, cavity_at(world_xz, terrain_height(world_xz)));
    }
    return vec4<f32>(oct_encode(splat.normal), splat.roughness, splat.material_id);
}
