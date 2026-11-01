// Analytic height fog in two exponential layers, mist and haze (Crytek/Wenzel, with
// one more layer). Imported by the terrain shader and by any material that fogs
// itself, in the lit colour before tonemapping.
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

// Henyey-Greenstein's phase function past its even share: the even share is in
// `color` and `haze_color`, what's left is the brightening around the sun.
fn sun_glow(cos_sun: f32, g: f32) -> f32 {
    const EVEN_SHARE: f32 = 0.07957747; // 1 / 4π
    let phase = EVEN_SHARE * (1.0 - g * g) / pow(1.0 + g * g - 2.0 * g * cos_sun, 1.5);
    return max(phase - EVEN_SHARE, 0.0);
}

// `color`, exposed by `exposure`, as seen from cam_pos through the fog to world_pos.
fn apply_height_fog(
    fog: HeightFog,
    color: vec3<f32>,
    cam_pos: vec3<f32>,
    world_pos: vec3<f32>,
    exposure: f32,
) -> vec3<f32> {
    // Beer-Lambert per channel: what's behind the fog fades, and what the fog
    // scatters toward the eye takes its place in the same measure.
    let layers = height_fog_layers(fog, cam_pos, world_pos);
    let mist = layers.x * fog.extinction;
    let haze = layers.y * fog.haze_extinction;
    let depth = mist + haze;
    let transmittance = exp(-depth);
    // Each layer lends the ray its colour in proportion to the depth it adds.
    let air = (mist * fog.color + haze * fog.haze_color) / max(depth, vec3<f32>(1e-30));
    let cos_sun = dot(normalize(world_pos - cam_pos), fog.sun_direction);
    let scattered = air + fog.sun_color * sun_glow(cos_sun, fog.sun_forward);
    return color * transmittance + scattered * exposure * (1.0 - transmittance);
}
