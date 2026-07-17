// Shared analytic exponential height fog (Crytek/Wenzel). Imported by both the
// fullscreen post-process (`height_fog.wgsl`, flatscreen tier, fogs the sky too)
// and the terrain shader (`terrain.wgsl`, VR tier, inline / virtually free but
// terrain-only), so the two tiers match.
#define_import_path bevy_wilderness::fog_functions

struct HeightFog {
    color: vec3<f32>,   // mist color, linear, exposure-applied HDR range (~0..1)
    density: f32,       // base density at base_height; 0 disables
    base_height: f32,   // world-Y the mist layer sits at
    falloff: f32,       // 1/m density drop per altitude (bigger = thinner, pools in valleys)
    max_distance: f32,  // saturation distance; also the distance used for sky pixels
    _pad: f32,
}

// 0 = clear, 1 = full fog, for the ray from cam_pos to world_pos.
fn height_fog_amount(fog: HeightFog, cam_pos: vec3<f32>, world_pos: vec3<f32>) -> f32 {
    let ray = world_pos - cam_pos;
    let d = length(ray);
    let dist = min(d, fog.max_distance);
    let dir_y = ray.y / max(d, 1e-4);
    // Clamp both exponents to a finite range: without it a high camera flushes `c`
    // to 0 while a steep down-ray overflows `g` to +inf, and 0·inf = NaN pixels.
    let c = fog.density * exp(clamp(-fog.falloff * (cam_pos.y - fog.base_height), -60.0, 60.0));
    // Optical depth = c·dist·(1-exp(-x))/x, x = dist·dir_y·falloff. Branch on x
    // (not dir_y): far rays make x non-negligible at any dir_y, so a dir_y-only
    // fallback steps at the horizon. (1-exp(-x))/x -> 1 as x -> 0.
    let x = clamp(dist * dir_y * fog.falloff, -60.0, 60.0);
    var g = 1.0;
    if abs(x) >= 1e-4 {
        g = (1.0 - exp(-x)) / x;
    }
    // Beer-Lambert fraction (smooth); clamping raw optical depth kinks at depth==1.
    return 1.0 - exp(-c * dist * g);
}

fn apply_height_fog(fog: HeightFog, color: vec3<f32>, cam_pos: vec3<f32>, world_pos: vec3<f32>) -> vec3<f32> {
    return mix(color, fog.color, height_fog_amount(fog, cam_pos, world_pos));
}
