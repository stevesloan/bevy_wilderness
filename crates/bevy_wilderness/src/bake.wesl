// Bakes the sun shadow-ceiling field: a texture indexed by world XZ, rendered by
// a top-down orthographic camera over a flat quad covering the terrain.
import bevy_pbr::render::forward_io::VertexOutput;

@group(constants::MATERIAL_BIND_GROUP) @binding(0) var heightmap_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var heightmap_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(2) var<uniform> texel_size: f32;
@group(constants::MATERIAL_BIND_GROUP) @binding(3) var<uniform> minmax: vec2<f32>;
// Normalized direction toward the fixed sun.
@group(constants::MATERIAL_BIND_GROUP) @binding(14) var<uniform> sun_direction: vec3<f32>;
// 1 = looping terrain: read the heightmap toroidally so the shadow march wraps
// across tile edges; 0 = finite (clamp).
@group(constants::MATERIAL_BIND_GROUP) @binding(15) var<uniform> looping: u32;

fn heightmap_uv(world_xz: vec2<f32>) -> vec2<f32> {
    let texture_size = vec2<f32>(textureDimensions(heightmap_texture));
    return world_xz / (texture_size * texel_size) + 0.5;
}

// Fetch one heightmap texel, wrapping toroidally when looping so the shadow
// march reads the tiled heightmap; otherwise clamp to the edge.
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

// World-space terrain height at a position.
fn terrain_height(world_xz: vec2<f32>) -> f32 {
    let h = height_bilinear(heightmap_uv(world_xz), 0);
    return h * (minmax.y - minmax.x) + minmax.x;
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

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let shadow = shadow_ceiling(in.world_position.xz);
    // R and B are biased by `minmax.x - 1` so R is always >= 1: the bake's
    // readiness sentinel treats an all-zero readback as "not drawn yet", and a
    // legitimately zero height would read as a stall forever. B is the column's
    // own ground, which R equals wherever the column is lit.
    let bias = minmax.x - 1.0;
    let ground = terrain_height(in.world_position.xz);
    return vec4<f32>(shadow.x - bias, shadow.y, ground - bias, 1.0);
}
