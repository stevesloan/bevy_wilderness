// Fills one distance slice of the fog shadow volume (`fog_shadow.rs`, and
// `fog_functions.wesl` for its layout): per direction from the viewer, the
// share of the fog's light toward the eye that the terrain's shadow has taken
// by that distance. Each slice adds its own stretch of ray to the one before,
// carried between passes in `carried`.

import bevy_wilderness::fog_functions::{
    HeightFog, fog_shadow_margin, green_depth, layers_along, sunlit_between, shadow_volume_direction,
    shadow_volume_reach,
};
import bevy_wilderness::sun_shadow::SunShadowParams;

struct Bake {
    fog: HeightFog,
    shadow: SunShadowParams,
    center: vec3<f32>,
    slice: u32,
    size: vec3<u32>,
    // Stretches the slice's own length of ray is taken in.
    steps: u32,
}

@group(0) @binding(0) var<uniform> bake: Bake;
@group(0) @binding(1) var field: texture_2d<f32>;
@group(0) @binding(2) var field_sampler: sampler;
// The slice before: r the share so far, g the shadow margin where it ended.
@group(1) @binding(0) var carried: texture_2d<f32>;

// A point under the ground takes its column's margin at the surface
// (`fog_shadow_margin`): a slice reaches past the surface it's read at, and
// what it holds there is filtered into the fog in front. Shaded, it would
// darken the fog over sunlit ground, and lit, brighten it over shaded ground.
fn margin_at(shadow: SunShadowParams, world_pos: vec3<f32>) -> f32 {
    return fog_shadow_margin(field, field_sampler, shadow, world_pos);
}

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32(index >> 1u), f32(index & 1u)) * 2.0;
    return vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
}

struct Slice {
    @location(0) volume: vec4<f32>,
    @location(1) carried: vec4<f32>,
}

@fragment
fn fragment(@builtin(position) position: vec4<f32>) -> Slice {
    // Adreno corrupts a uniform struct passed by value, so functions get a copy
    // built field by field.
    let fog = HeightFog(
        bake.fog.color,
        bake.fog.density,
        bake.fog.extinction,
        bake.fog.base_height,
        bake.fog.sun_color,
        bake.fog.falloff,
        bake.fog.sun_direction,
        bake.fog.max_distance,
        bake.fog.haze_color,
        bake.fog.molecule_density,
        bake.fog.molecule_extinction,
        bake.fog.molecule_falloff,
        bake.fog.start,
        bake.fog.sun_forward,
        bake.fog.aerosol_density,
        bake.fog.aerosol_falloff,
        bake.fog.shadowed,
    );
    let shadow = SunShadowParams(
        bake.shadow.inv_world_size,
        bake.shadow.height_bias,
        bake.shadow.penumbra_per_metre,
        bake.shadow.valid,
        bake.shadow.top,
    );
    let cell = vec2<u32>(position.xy);
    let dir = shadow_volume_direction(cell, bake.size.xy);
    let start = bake.center + dir * fog.start;
    let reach = shadow_volume_reach(fog, shadow);

    let last = f32(bake.size.z - 1u);
    let near = f32(bake.slice - 1u) / last;
    let far = f32(bake.slice) / last;
    var before = reach * near * near;
    var shaded = 0.0;
    var margin_before = 0.0;
    if bake.slice == 1u {
        margin_before = margin_at(shadow, start);
    } else {
        let previous = textureLoad(carried, vec2<i32>(cell), 0);
        shaded = previous.r;
        margin_before = previous.g;
    }
    for (var i = 1u; i <= bake.steps; i++) {
        let w = mix(near, far, f32(i) / f32(bake.steps));
        let end = reach * w * w;
        let margin = margin_at(shadow, start + dir * end);
        // In closed form, not from a point: mist is a thin layer, which a
        // stretch's middle can miss.
        let depth = green_depth(fog, layers_along(fog, start.y, before, dir.y));
        let origin_y = start.y + dir.y * before;
        let stretch = green_depth(fog, layers_along(fog, origin_y, end - before, dir.y));
        shaded += exp(-depth) * (1.0 - exp(-stretch)) * (1.0 - sunlit_between(margin_before, margin));
        before = end;
        margin_before = margin;
    }
    return Slice(vec4<f32>(shaded, 0.0, 0.0, 1.0), vec4<f32>(shaded, margin_before, 0.0, 1.0));
}
