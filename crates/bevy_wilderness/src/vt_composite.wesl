// Composites the terrain splat into one rectangle of a texture-clipmap level:
// albedo + cavity into the first target, octahedral normal + roughness + packed
// material ids into the second. Drawn as a fullscreen triangle clipped by the
// viewport to the rectangle's slots in the level's array layer; see `vt.rs`.
//
// A fragment's world position is linear in its framebuffer position (`draw`),
// so the implicit derivatives the layer samples take are the level's texel
// size: every level is prefiltered at its own density.

@group(0) @binding(0) var heightmap_texture: texture_2d<f32>;
@group(0) @binding(1) var albedo_array: texture_2d_array<f32>;
@group(0) @binding(2) var albedo_sampler: sampler;
@group(0) @binding(3) var normal_array: texture_2d_array<f32>;
@group(0) @binding(4) var normal_sampler: sampler;
@group(0) @binding(5) var orm_array: texture_2d_array<f32>;
@group(0) @binding(6) var orm_sampler: sampler;
@group(0) @binding(7) var<uniform> params: TerrainParams;
@group(0) @binding(8) var<uniform> terrain: CompositeTerrain;
@group(1) @binding(0) var<uniform> draw: CompositeDraw;

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

// Matches `CompositeTerrain` in `vt/render.rs`.
struct CompositeTerrain {
    minmax: vec2<f32>,
    // Heightmap texel size, metres.
    texel_size: f32,
    // 1 = looping terrain: heightmap reads wrap toroidally.
    looping: u32,
}

// Matches `CompositeDraw` in `vt/render.rs`.
struct CompositeDraw {
    // World XZ of framebuffer (0, 0) in this layer for this draw.
    origin: vec2<f32>,
    // The level's texel size, metres.
    texel: f32,
    _pad: f32,
}

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
    // Top-2 layer ids + blend, packed into 8 bits (2+2+4); the main pass
    // unpacks it to blend per-material detail.
    material_id: f32,
}

fn heightmap_uv(world_xz: vec2<f32>) -> vec2<f32> {
    let texture_size = vec2<f32>(textureDimensions(heightmap_texture));
    return world_xz / (texture_size * terrain.texel_size) + 0.5;
}

fn load_height_texel(p: vec2<i32>, size: vec2<i32>) -> f32 {
    var idx = clamp(p, vec2(0), size - 1);
    if terrain.looping != 0u {
        idx = ((p % size) + size) % size;
    }
    return textureLoad(heightmap_texture, idx, 0).r;
}

fn height_bilinear(uv: vec2<f32>) -> f32 {
    let tex_size = vec2<i32>(textureDimensions(heightmap_texture));
    let pos = uv * vec2<f32>(tex_size);
    let p0 = vec2<i32>(floor(pos));
    let f = pos - floor(pos);
    let h00 = load_height_texel(p0, tex_size);
    let h10 = load_height_texel(p0 + vec2(1, 0), tex_size);
    let h01 = load_height_texel(p0 + vec2(0, 1), tex_size);
    let h11 = load_height_texel(p0 + vec2(1, 1), tex_size);
    return mix(mix(h00, h10, f.x), mix(h01, h11, f.x), f.y);
}

fn terrain_height(world_xz: vec2<f32>) -> f32 {
    let h = height_bilinear(heightmap_uv(world_xz));
    return h * (terrain.minmax.y - terrain.minmax.x) + terrain.minmax.x;
}

// Central differences across `step` metres. A coarse level passes its own texel
// size, so its normal is the slope averaged over the texel rather than a point
// sample of the full-resolution heightmap, which would alias.
fn geo_normal(world_xz: vec2<f32>, step: f32) -> vec3<f32> {
    let h_r = terrain_height(world_xz + vec2(step, 0.0));
    let h_l = terrain_height(world_xz - vec2(step, 0.0));
    let h_t = terrain_height(world_xz + vec2(0.0, step));
    let h_b = terrain_height(world_xz - vec2(0.0, step));
    let dh_dx = (h_r - h_l) / (2.0 * step);
    let dh_dy = (h_t - h_b) / (2.0 * step);
    return normalize(vec3(-dh_dx, 1.0, -dh_dy));
}

// Curvature as a depression angle: <0.5 concave, >0.5 convex, 0.5 flat.
fn cavity_at(world_xz: vec2<f32>, origin_h: f32) -> f32 {
    const CAVITY_RING_TAPS = 8;
    const CAVITY_SATURATION = 0.2679;  // tan(15°)
    let r = max(2.0 * terrain.texel_size, 16.0);
    var ring = 0.0;
    for (var k = 0; k < CAVITY_RING_TAPS; k++) {
        let a = f32(k) * (6.2831853 / f32(CAVITY_RING_TAPS));
        ring += terrain_height(world_xz + vec2<f32>(cos(a), sin(a)) * r);
    }
    let depression = (ring / f32(CAVITY_RING_TAPS) - origin_h) / r;
    return clamp(0.5 - 0.5 * depression / CAVITY_SATURATION, 0.0, 1.0);
}

// --- Stochastic hex tiling (Mikkelsen) ---

fn hex_hash(p: vec2<f32>) -> vec2<f32> {
    let r = vec2<f32>(dot(p, vec2<f32>(127.1, 311.7)), dot(p, vec2<f32>(269.5, 183.3)));
    return fract(sin(r) * 43758.5453);
}

fn triangle_grid(
    uv: vec2<f32>,
    w: ptr<function, vec3<f32>>,
    v1: ptr<function, vec2<f32>>,
    v2: ptr<function, vec2<f32>>,
    v3: ptr<function, vec2<f32>>,
) {
    // Cells span ~1 texture repeat: smaller ones speckle under minification,
    // larger ones show repetition.
    let skewed = vec2<f32>(uv.x - 0.57735027 * uv.y, 1.15470054 * uv.y);
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
    // Per-cell offsets break implicit derivatives, hence the explicit ones.
    let c1 = textureSampleGrad(tex, samp, uv + hex_hash(v1), layer, ddx, ddy);
    let c2 = textureSampleGrad(tex, samp, uv + hex_hash(v2), layer, ddx, ddy);
    let c3 = textureSampleGrad(tex, samp, uv + hex_hash(v3), layer, ddx, ddy);
    // Sharpened weights keep transitions crisp (no ghosting).
    let ws = pow(w, vec3<f32>(7.0)) + vec3<f32>(1e-6);
    let wn = ws / (ws.x + ws.y + ws.z);
    return c1 * wn.x + c2 * wn.y + c3 * wn.z;
}

fn splat_terrain(world_xz: vec2<f32>, normal: vec3<f32>, height: f32, cavity: f32) -> SplatResult {
    let slope = acos(clamp(normal.y, -1.0, 1.0));

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
        // normal's red axis is mirrored — cracks would light up as bumps.
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
    // The blend `t2` (0..1) maps to a detail-normal lerp of 0..0.5 in the main pass.
    let t2 = clamp(2.0 * b1 / max(b0 + b1, 1e-4), 0.0, 1.0);
    let q = floor(t2 * 15.0 + 0.5);
    out.material_id = (f32(i0) + f32(i1) * 4.0 + q * 16.0) / 255.0;
    return out;
}

fn oct_wrap(v: vec2<f32>) -> vec2<f32> {
    return (1.0 - abs(v.yx)) * select(vec2(-1.0), vec2(1.0), v >= vec2(0.0));
}

fn oct_encode(n: vec3<f32>) -> vec2<f32> {
    let m = abs(n.x) + abs(n.y) + abs(n.z);
    var v = n.xy / m;
    v = select(oct_wrap(v), v, n.z >= 0.0);
    return v * 0.5 + 0.5;
}

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32(index >> 1u), f32(index & 1u)) * 2.0;
    return vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
}

struct CompositeOutput {
    @location(0) albedo: vec4<f32>,
    @location(1) normal: vec4<f32>,
}

@fragment
fn fragment(@builtin(position) position: vec4<f32>) -> CompositeOutput {
    let world_xz = draw.origin + position.xy * draw.texel;
    let height = terrain_height(world_xz);
    let cavity = cavity_at(world_xz, height);
    let normal = geo_normal(world_xz, max(draw.texel, terrain.texel_size));
    let splat = splat_terrain(world_xz, normal, height, cavity);
    var out: CompositeOutput;
    out.albedo = vec4<f32>(splat.color, cavity);
    out.normal = vec4<f32>(oct_encode(splat.normal), splat.roughness, splat.material_id);
    return out;
}
