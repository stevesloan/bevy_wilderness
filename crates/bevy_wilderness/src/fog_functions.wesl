// Analytic height fog in two exponential layers, mist and haze (Crytek/Wenzel, with
// one more layer). Imported by the terrain shader and by any material that fogs
// itself, in the lit colour before tonemapping.

import bevy_wilderness::sun_shadow::{SunShadowParams, sun_shadow_margin};

struct HeightFog {
    color: vec3<f32>,         // light the mist scatters toward the eye, cd/m²
    density: f32,             // mist's density at base_height; 0 for none
    extinction: vec3<f32>,    // mist's density's share per channel
    base_height: f32,         // world-Y both layers are measured from
    sun_color: vec3<f32>,     // sunlight reaching the fog, lux
    falloff: f32,             // 1/m mist's density drop per altitude (bigger = thinner, pools in valleys)
    sun_direction: vec3<f32>, // unit, toward the sun
    max_distance: f32,        // saturation distance; also the distance used for sky pixels
    haze_color: vec3<f32>,    // light the haze scatters toward the eye, cd/m²
    haze_density: f32,        // haze's density at base_height; 0 for none
    haze_extinction: vec3<f32>, // haze's density's share per channel
    haze_falloff: f32,        // 1/m haze's density drop per altitude, far gentler than mist's
    start: f32,               // m from the eye before any fog
    sun_forward: f32,         // Henyey-Greenstein g: 0 scatters sunlight evenly, 1 straight on
    haze_aerosol: vec3<f32>,  // aerosols' share of the haze's scattering per channel; molecules the rest
    shadow_samples: u32,      // points per ray testing the sun shadow field; 0 for none
}

// Optical depth of one exponential layer along a ray of `dist` metres from `origin_y`.
fn layer_depth(density: f32, falloff: f32, base_height: f32, origin_y: f32, dist: f32, dir_y: f32) -> f32 {
    // Clamp both exponents to a finite range: without it a high camera flushes `c`
    // to 0 while a steep down-ray overflows `g` to +inf, and 0·inf = NaN pixels.
    let c = density * exp(clamp(-falloff * (origin_y - base_height), -60.0, 60.0));
    // Optical depth = c·dist·(1-exp(-x))/x, x = dist·dir_y·falloff. Branch on x
    // (not dir_y): far rays make x non-negligible at any dir_y, so a dir_y-only
    // fallback steps at the horizon. (1-exp(-x))/x -> 1 as x -> 0.
    let x = clamp(dist * dir_y * falloff, -60.0, 60.0);
    var g = 1.0;
    if abs(x) >= 1e-4 {
        g = (1.0 - exp(-x)) / x;
    }
    return c * dist * g;
}

// Optical depth of the ray from cam_pos to world_pos past `start`, mist then haze.
// Mist pools low and haze hangs at every altitude: no one exponential does both.
fn height_fog_layers(fog: HeightFog, cam_pos: vec3<f32>, world_pos: vec3<f32>) -> vec2<f32> {
    let ray = world_pos - cam_pos;
    let d = length(ray);
    let dir_y = ray.y / max(d, 1e-4);
    let skipped = min(fog.start, d);
    let dist = min(d - skipped, fog.max_distance);
    let origin_y = cam_pos.y + dir_y * skipped;
    return vec2<f32>(
        layer_depth(fog.density, fog.falloff, fog.base_height, origin_y, dist, dir_y),
        layer_depth(fog.haze_density, fog.haze_falloff, fog.base_height, origin_y, dist, dir_y),
    );
}

// How much of what's behind the fog survives it, per channel.
fn height_fog_transmittance(fog: HeightFog, cam_pos: vec3<f32>, world_pos: vec3<f32>) -> vec3<f32> {
    let layers = height_fog_layers(fog, cam_pos, world_pos);
    return exp(-layers.x * fog.extinction - layers.y * fog.haze_extinction);
}

// Phase functions past their even share: the even share is in `color` and
// `haze_color`, what's left is the brightening around the sun.
const EVEN_SHARE: f32 = 0.07957747; // 1 / 4π

// Droplets and aerosols (Henyey-Greenstein): a strong glow toward the sun.
fn forward_glow(cos_sun: f32, g: f32) -> f32 {
    let phase = EVEN_SHARE * (1.0 - g * g) / pow(1.0 + g * g - 2.0 * g * cos_sun, 1.5);
    return max(phase - EVEN_SHARE, 0.0);
}

// Molecules (Rayleigh): close to even, a little brighter toward and away from the sun.
fn molecular_glow(cos_sun: f32) -> f32 {
    return max(0.75 * EVEN_SHARE * (1.0 + cos_sun * cos_sun) - EVEN_SHARE, 0.0);
}

// The integral of the sun's share over a shadow margin, where the share ramps
// from 0 at -1 to 1 at 1 (`sun_shadow_margin`).
fn sunlit_integral(margin: f32) -> f32 {
    if margin <= -1.0 {
        return 0.0;
    }
    if margin >= 1.0 {
        return margin;
    }
    return (margin + 1.0) * (margin + 1.0) * 0.25;
}

// The sun's share along a stretch of ray whose margin runs linearly from `a` to
// `b`. Taking it over the stretch rather than at its ends is what keeps the fog
// from banding: the share moves smoothly as a shadow's edge passes a sample
// point, where the points' own 0 or 1 would step together across the screen.
fn sunlit_between(a: f32, b: f32) -> f32 {
    if abs(b - a) < 1e-3 {
        return clamp(0.25 * (a + b) + 0.5, 0.0, 1.0);
    }
    return (sunlit_integral(b) - sunlit_integral(a)) / (b - a);
}

// Metres above the terrain's top a summit's penumbra can still reach.
const TOP_PENUMBRA: f32 = 25.0;

// How far along a ray from `origin` the terrain can shade it: until it leaves
// the field's square or climbs over the terrain's top.
fn shadowable_length(params: SunShadowParams, origin: vec3<f32>, dir: vec3<f32>) -> f32 {
    let half = 0.5 / max(params.inv_world_size, 1e-9);
    var t = 1e30;
    if abs(dir.x) > 1e-6 {
        t = min(t, (sign(dir.x) * half - origin.x) / dir.x);
    }
    if abs(dir.z) > 1e-6 {
        t = min(t, (sign(dir.z) * half - origin.z) / dir.z);
    }
    if dir.y > 1e-6 {
        t = min(t, (params.top + TOP_PENUMBRA - origin.y) / dir.y);
    }
    return max(t, 0.0);
}

// The share of the fog's light between cam_pos and world_pos that the sun
// reaches, from the terrain's sun shadow field along the ray:
// `fog.shadow_samples` stretches, each weighted by the light it sends the eye,
// its own fog dimmed by the fog in front of it, in green, the channel
// visibility is quoted for. The stretches cover only the part of the ray the
// terrain can shade, the rest counting as sunlit, and lengthen away from the
// eye, quadratically: near fog shows a shaft's edge sharply, and far fog is
// mostly haze, which varies slowly. 1 with no samples.
fn fog_sunlit_share(
    fog: HeightFog,
    field: texture_2d<f32>,
    field_sampler: sampler,
    params: SunShadowParams,
    cam_pos: vec3<f32>,
    world_pos: vec3<f32>,
) -> f32 {
    if fog.shadow_samples == 0u || params.valid < 0.5 {
        return 1.0;
    }
    let ray = world_pos - cam_pos;
    let d = length(ray);
    let dir = ray / max(d, 1e-4);
    let skipped = min(fog.start, d);
    let dist = min(d - skipped, fog.max_distance);
    let start = cam_pos + dir * skipped;
    let span = min(dist, shadowable_length(params, start, dir));
    let samples = f32(fog.shadow_samples);
    let mist = fog.density * fog.extinction.y;
    let haze = fog.haze_density * fog.haze_extinction.y;
    var lit = 0.0;
    // Optical depth from the start to the current stretch.
    var depth = 0.0;
    var before = 0.0;
    var margin_before = sun_shadow_margin(field, field_sampler, params, start);
    for (var i = 1u; i <= fog.shadow_samples; i++) {
        let u = f32(i) / samples;
        let margin = sun_shadow_margin(field, field_sampler, params, start + dir * (span * u * u));
        // In closed form, not from a point: mist is a thin layer, which a
        // stretch's middle can miss.
        let origin_y = start.y + dir.y * span * before * before;
        let run = span * (u * u - before * before);
        let stretch = layer_depth(mist, fog.falloff, fog.base_height, origin_y, run, dir.y)
            + layer_depth(haze, fog.haze_falloff, fog.base_height, origin_y, run, dir.y);
        let weight = exp(-depth) * (1.0 - exp(-stretch));
        lit += weight * sunlit_between(margin_before, margin);
        depth += stretch;
        before = u;
        margin_before = margin;
    }
    let rest = layer_depth(mist, fog.falloff, fog.base_height, start.y + dir.y * span, dist - span, dir.y)
        + layer_depth(haze, fog.haze_falloff, fog.base_height, start.y + dir.y * span, dist - span, dir.y);
    lit += exp(-depth) * (1.0 - exp(-rest));
    depth += rest;
    let total = 1.0 - exp(-depth);
    if total <= 1e-6 {
        return 1.0;
    }
    return lit / total;
}

// `color`, exposed by `exposure`, as seen from cam_pos through the fog to world_pos.
fn apply_height_fog(
    fog: HeightFog,
    color: vec3<f32>,
    cam_pos: vec3<f32>,
    world_pos: vec3<f32>,
    exposure: f32,
) -> vec3<f32> {
    return apply_height_fog_lit(fog, color, cam_pos, world_pos, exposure, 1.0);
}

// `apply_height_fog` with `sunlit` of the fog reached by the sun
// (`fog_sunlit_share`): the rest loses its glow toward the sun and the mist the
// sun's even share of its colour, keeping only the sky's light.
fn apply_height_fog_lit(
    fog: HeightFog,
    color: vec3<f32>,
    cam_pos: vec3<f32>,
    world_pos: vec3<f32>,
    exposure: f32,
    sunlit: f32,
) -> vec3<f32> {
    // Beer-Lambert per channel: what's behind the fog fades, and what the fog
    // scatters toward the eye takes its place in the same measure.
    let layers = height_fog_layers(fog, cam_pos, world_pos);
    let mist = layers.x * fog.extinction;
    let haze = layers.y * fog.haze_extinction;
    let depth = mist + haze;
    let transmittance = exp(-depth);
    // Each layer lends the ray its colour in proportion to the depth it adds.
    let mist_color = max(fog.color - (1.0 - sunlit) * fog.sun_color * EVEN_SHARE, vec3<f32>(0.0));
    let air = (mist * mist_color + haze * fog.haze_color) / max(depth, vec3<f32>(1e-30));
    // Mist is droplets throughout; haze is part aerosol, part molecules.
    let cos_sun = dot(normalize(world_pos - cam_pos), fog.sun_direction);
    let forward = forward_glow(cos_sun, fog.sun_forward);
    let haze_glow = mix(vec3<f32>(molecular_glow(cos_sun)), vec3<f32>(forward), fog.haze_aerosol);
    let glow = (mist * forward + haze * haze_glow) / max(depth, vec3<f32>(1e-30));
    let scattered = air + fog.sun_color * glow * sunlit;
    return color * transmittance + scattered * exposure * (1.0 - transmittance);
}
