// Bakes the terrain splat material into a texture indexed by world XZ. Runs the
// same splat blend as the main pass but writes raw channels unlit, so the main
// pass can sample the result instead of blending per-fragment. Rendered by a
// top-down orthographic camera over a flat quad covering the terrain.
#import bevy_pbr::forward_io::VertexOutput

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var heightmap_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var heightmap_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> texel_size: f32;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var<uniform> minmax: vec2<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(4) var albedo_array: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(5) var albedo_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(8) var<uniform> params: TerrainParams;
@group(#{MATERIAL_BIND_GROUP}) @binding(9) var normal_array: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(10) var normal_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(11) var orm_array: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(12) var orm_sampler: sampler;
// 0 = albedo target (rgb albedo, a sun-visibility); 1 = normal target (rg
// octahedral world normal, b roughness, a = packed top-2 material ids + blend).
@group(#{MATERIAL_BIND_GROUP}) @binding(13) var<uniform> output_mode: u32;
// Normalized direction toward the fixed sun.
@group(#{MATERIAL_BIND_GROUP}) @binding(14) var<uniform> sun_direction: vec3<f32>;
// 1 = looping terrain: read the heightmap toroidally so the shadow/AO marches
// wrap across tile edges (baked shadows tile seamlessly); 0 = finite (clamp).
@group(#{MATERIAL_BIND_GROUP}) @binding(15) var<uniform> looping: u32;

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

// Fetch one heightmap texel, wrapping toroidally when looping so the shadow/AO
// marches read the tiled heightmap; otherwise clamp to the edge.
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

// Terrain self-shadow: soft-march the heightmap toward the sun. 0 = shadowed,
// 1 = lit. Baked once (static terrain, fixed sun), so the march is affordable.
fn sun_visibility(world_xz: vec2<f32>) -> f32 {
    const STEPS = 96;
    const MAX_DIST = 6000.0;
    const SOFTNESS = 20.0;      // lower = softer penumbra
    const GROWTH = 1.12;        // geometric growth -> long reach without huge step count
    let NORMAL_BIAS = texel_size * 1.0;   // lift the ray off the surface to avoid acne
    let STEP0 = texel_size * 1.0;         // fine near-field step (resolves steep sun-facing slopes)
    // Bias the start along the surface normal so sun-facing slopes don't
    // self-shadow.
    let origin = vec3<f32>(world_xz.x, terrain_height(world_xz), world_xz.y)
        + geo_normal(world_xz) * NORMAL_BIAS;
    var vis = 1.0;
    var step = STEP0;
    var t = STEP0;
    for (var i = 0; i < STEPS; i++) {
        if t > MAX_DIST {
            break;
        }
        let p = origin + sun_direction * t;
        if p.y > minmax.y {
            break; // above the highest terrain -> can't be occluded
        }
        // Soft shadow: penumbra widens with distance to the occluder.
        let clearance = p.y - terrain_height(p.xz);
        vis = min(vis, clamp(SOFTNESS * clearance / t, 0.0, 1.0));
        if vis <= 0.001 {
            break;
        }
        step *= GROWTH;
        t += step;
    }
    return vis;
}

// Macro ambient occlusion + bent normal + curvature, gathered from the
// heightfield. Horizon-based: for each azimuth, march outward and track the
// highest occluding horizon angle; the open sky above it drives AO and the
// average unoccluded direction (the bent normal). Curvature comes from a small
// height-Laplacian neighborhood. All sun-independent and baked once (mode 2) —
// never per-frame. This is the single heaviest bake pass; DIRS/STEPS are the
// cost knobs (raise DIRS for smoother bent normals if the bake completes
// comfortably; lower both, or drop RVT_SIZE, if the driver times the bake out).
fn ambient_gather(world_xz: vec2<f32>) -> vec4<f32> {
    const DIRS = 8;          // azimuth samples (bent-normal smoothness)
    const STEPS = 12;        // march samples per azimuth (occlusion reach)
    const MAX_R = 400.0;     // occlusion search radius, world units (local by nature)
    const R0 = 4.0;
    const GROWTH = 1.25;     // geometric step growth -> reach without step count
    const TAU = 6.2831853;
    const HALF_PI = 1.5707963;

    let origin_h = terrain_height(world_xz);
    var ao = 0.0;
    var bent = vec3<f32>(0.0);
    for (var d = 0; d < DIRS; d++) {
        let phi = TAU * f32(d) / f32(DIRS);
        let dir = vec2<f32>(cos(phi), sin(phi));
        // Highest horizon tangent (rise/run) seen along this azimuth.
        var max_tan = 0.0;
        var t = R0;
        var step = R0;
        for (var s = 0; s < STEPS; s++) {
            if t > MAX_R {
                break;
            }
            let dh = terrain_height(world_xz + dir * t) - origin_h;
            max_tan = max(max_tan, dh / t);
            step *= GROWTH;
            t += step;
        }
        let horizon = atan(max_tan);            // elevation of the horizon (>= 0)
        // Cosine-weighted open sky in this wedge over a flat upper hemisphere
        // integrates to ~1 - sin(horizon): flat ground (horizon 0) -> 1.
        let vis = 1.0 - sin(max(horizon, 0.0));
        ao += vis;
        // Average unoccluded direction: aim at the middle of the open wedge.
        let mid = 0.5 * (horizon + HALF_PI);
        bent += vec3<f32>(dir.x * cos(mid), sin(mid), dir.y * cos(mid)) * vis;
    }
    ao /= f32(DIRS);
    let bent_n = normalize(bent + vec3<f32>(0.0, 1e-3, 0.0));

    // Curvature / cavity from the height Laplacian: concave (valleys, cracks) vs
    // convex (ridges, ledges). >0.5 convex, <0.5 concave, 0.5 flat. The 8.0
    // scale is a look knob — tune to the terrain's height range.
    let e = texel_size * 2.0;
    let hr = terrain_height(world_xz + vec2<f32>(e, 0.0));
    let hl = terrain_height(world_xz - vec2<f32>(e, 0.0));
    let ht = terrain_height(world_xz + vec2<f32>(0.0, e));
    let hb = terrain_height(world_xz - vec2<f32>(0.0, e));
    let lap = (hr + hl + ht + hb - 4.0 * origin_h) / e;
    let cavity = clamp(0.5 - lap * 8.0, 0.0, 1.0);

    // R = AO, GB = bent normal world X/Z (Y reconstructed in the main pass),
    // A = cavity.
    return vec4<f32>(ao, bent_n.x * 0.5 + 0.5, bent_n.z * 0.5 + 0.5, cavity);
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
// baked result. Procedural placement (§3.2) + hex de-tiling.
fn splat_terrain(world_xz: vec2<f32>, normal: vec3<f32>) -> SplatResult {
    // Procedural placement: each layer's weight is the overlap of its slope band
    // (grass→dirt→rock) and its world-height band (e.g. snow above a snowline).
    let slope = acos(clamp(normal.y, -1.0, 1.0));
    let height = terrain_height(world_xz);

    var w = array<f32, 4>(0.0, 0.0, 0.0, 0.0);
    for (var i = 0u; i < MAX_LAYERS; i++) {
        if i >= params.layer_count {
            continue;
        }
        w[i] = band(slope, params.slope_min[i], params.slope_max[i], params.slope_blend[i])
             * band(height, params.height_min[i], params.height_max[i], params.height_range_blend[i]);
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
    // Mode 2 skips the splat entirely — only the heightfield gather.
    if output_mode == 2u {
        return ambient_gather(world_xz);
    }
    let splat = splat_terrain(world_xz, geo_normal(world_xz));
    if output_mode == 0u {
        return vec4<f32>(splat.color, sun_visibility(world_xz));
    }
    return vec4<f32>(oct_encode(splat.normal), splat.roughness, splat.material_id);
}
