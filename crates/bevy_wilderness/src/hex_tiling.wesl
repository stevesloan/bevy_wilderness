// Texture-projection helpers shared by the RVT bake and the main pass's detail
// overlay: stochastic hex tiling and triplanar mapping.
//
// Stochastic hex tiling (Mikkelsen, "Practical Real-Time Hex-Tiling"): samples
// a tiled texture 3× on a randomized hex lattice and blends, hiding the
// repetition. Used by the RVT bake (every layer, once) and by the main pass's
// near detail overlay when `TerrainQuality::detail_hex_tiling` is on. Per-cell
// offsets break implicit derivatives, so every tap is `textureSampleGrad`.

// The three lattice taps around a point — computed once and shared by every
// texture sampled at the same UV (albedo / normal / ORM).
struct HexTaps {
    o1: vec2<f32>,
    o2: vec2<f32>,
    o3: vec2<f32>,
    // Sharpened, normalized barycentric weights.
    w: vec3<f32>,
}

fn hex_hash(p: vec2<f32>) -> vec2<f32> {
    let r = vec2<f32>(dot(p, vec2<f32>(127.1, 311.7)), dot(p, vec2<f32>(269.5, 183.3)));
    return fract(sin(r) * 43758.5453);
}

// Cells span ~1 texture repeat: each cell is a differently-offset crop.
// Smaller cells speckle under minification; larger ones show repetition.
fn hex_taps(uv: vec2<f32>) -> HexTaps {
    // Skewed triangle (hex) grid: barycentric weights + the 3 cell vertices.
    let skewed = vec2<f32>(uv.x - 0.57735027 * uv.y, 1.15470054 * uv.y);
    let base = floor(skewed);
    let f = fract(skewed);
    let fz = 1.0 - f.x - f.y;
    let s = step(0.0, -fz);
    let s2 = 2.0 * s - 1.0;
    let w = vec3<f32>(-fz * s2, s - f.y * s2, s - f.x * s2);
    // Sharpen the barycentric weights to keep transitions crisp (avoid ghosting).
    let ws = pow(w, vec3<f32>(7.0)) + vec3<f32>(1e-6);
    var taps: HexTaps;
    taps.o1 = hex_hash(base + vec2<f32>(s, s));
    taps.o2 = hex_hash(base + vec2<f32>(s, 1.0 - s));
    taps.o3 = hex_hash(base + vec2<f32>(1.0 - s, s));
    taps.w = ws / (ws.x + ws.y + ws.z);
    return taps;
}

fn hex_sample_taps(
    tex: texture_2d_array<f32>,
    samp: sampler,
    layer: u32,
    uv: vec2<f32>,
    taps: HexTaps,
    ddx: vec2<f32>,
    ddy: vec2<f32>,
) -> vec4<f32> {
    let c1 = textureSampleGrad(tex, samp, uv + taps.o1, layer, ddx, ddy);
    let c2 = textureSampleGrad(tex, samp, uv + taps.o2, layer, ddx, ddy);
    let c3 = textureSampleGrad(tex, samp, uv + taps.o3, layer, ddx, ddy);
    return c1 * taps.w.x + c2 * taps.w.y + c3 * taps.w.z;
}

fn hex_sample(
    tex: texture_2d_array<f32>,
    samp: sampler,
    layer: u32,
    uv: vec2<f32>,
    ddx: vec2<f32>,
    ddy: vec2<f32>,
) -> vec4<f32> {
    return hex_sample_taps(tex, samp, layer, uv, hex_taps(uv), ddx, ddy);
}

// --- Triplanar on steep ground ---
// Projecting textures straight down (`world_xz / tiling`) smears them along a
// slope by 1/cos(slope): 2× at 60°, ~4× at 75°. Rock lives on steep ground, so
// it was always the most stretched material. Steep surfaces also project from
// the two sides (X and Z), blended by surface angle; a projection whose weight
// is below `TRIPLANAR_MIN` isn't sampled, so flat ground pays nothing.
const TRIPLANAR_SHARPNESS: f32 = 4.0;
const TRIPLANAR_MIN: f32 = 0.02;

// Projection weights (top, side X, side Z) for surface normal `n`.
fn triplanar_weights(n: vec3<f32>) -> vec3<f32> {
    let w = pow(abs(n), vec3<f32>(TRIPLANAR_SHARPNESS));
    let top_x_z = vec3<f32>(w.y, w.x, w.z) / (w.x + w.y + w.z);
    // Drop negligible projections, renormalizing the rest.
    let kept = select(vec3<f32>(0.0), top_x_z, top_x_z >= vec3<f32>(TRIPLANAR_MIN));
    return kept / (kept.x + kept.y + kept.z);
}

// A side projection's normal-map sample `s` (unpacked, -1..1) as a world
// normal, by the whiteout blend (Golus, "Normal Mapping for a Triplanar
// Shader"); `axis` 1 = the X-facing plane (uv = z, -y), 2 = Z-facing (x, -y).
fn side_normal_world(s: vec3<f32>, n: vec3<f32>, axis: u32) -> vec3<f32> {
    if axis == 1u {
        return vec3<f32>(s.xy + n.zy, abs(s.z) * n.x).zyx;
    }
    return vec3<f32>(s.xy + n.xy, abs(s.z) * n.z);
}
