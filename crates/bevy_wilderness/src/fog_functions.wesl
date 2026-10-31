// Analytic exponential height fog (Crytek/Wenzel). Imported by the terrain shader
// and by any material that fogs itself, in the lit colour before tonemapping.
struct HeightFog {
    color: vec3<f32>,         // light the fog scatters toward the eye, cd/m²
    density: f32,             // base density at base_height; 0 disables
    extinction: vec3<f32>,    // density's share per channel: blue highest drifts distance blue
    base_height: f32,         // world-Y the mist layer sits at
    sun_color: vec3<f32>,     // cd/m² added looking straight at the sun
    falloff: f32,             // 1/m density drop per altitude (bigger = thinner, pools in valleys)
    sun_direction: vec3<f32>, // unit, toward the sun
    max_distance: f32,        // saturation distance; also the distance used for sky pixels
    start: f32,               // m from the eye before any fog
    sun_exponent: f32,        // how tight the glow around the sun is
}

// Optical depth of the ray from cam_pos to world_pos, past `start`.
fn height_fog_depth(fog: HeightFog, cam_pos: vec3<f32>, world_pos: vec3<f32>) -> f32 {
    let ray = world_pos - cam_pos;
    let d = length(ray);
    let dir_y = ray.y / max(d, 1e-4);
    let skipped = min(fog.start, d);
    let dist = min(d - skipped, fog.max_distance);
    let origin_y = cam_pos.y + dir_y * skipped;
    // Clamp both exponents to a finite range: without it a high camera flushes `c`
    // to 0 while a steep down-ray overflows `g` to +inf, and 0·inf = NaN pixels.
    let c = fog.density * exp(clamp(-fog.falloff * (origin_y - fog.base_height), -60.0, 60.0));
    // Optical depth = c·dist·(1-exp(-x))/x, x = dist·dir_y·falloff. Branch on x
    // (not dir_y): far rays make x non-negligible at any dir_y, so a dir_y-only
    // fallback steps at the horizon. (1-exp(-x))/x -> 1 as x -> 0.
    let x = clamp(dist * dir_y * fog.falloff, -60.0, 60.0);
    var g = 1.0;
    if abs(x) >= 1e-4 {
        g = (1.0 - exp(-x)) / x;
    }
    return c * dist * g;
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
    let transmittance = exp(-height_fog_depth(fog, cam_pos, world_pos) * fog.extinction);
    let toward_sun = saturate(dot(normalize(world_pos - cam_pos), fog.sun_direction));
    let scattered = fog.color + fog.sun_color * pow(toward_sun, fog.sun_exponent);
    return color * transmittance + scattered * exposure * (1.0 - transmittance);
}
