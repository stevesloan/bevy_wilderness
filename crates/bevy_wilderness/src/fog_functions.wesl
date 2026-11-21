// Analytic height fog in exponential layers: mist, and the haze of the air
// itself, molecules and aerosols each thinning at their own height (Crytek/
// Wenzel, with more layers). Imported by the terrain shader and by any material
// that fogs itself, in the lit colour before tonemapping.

import bevy_wilderness::sun_shadow::{SunShadowParams, MIN_PENUMBRA};

struct HeightFog {
    color: vec3<f32>,         // light the mist scatters toward the eye, cd/m²
    density: f32,             // mist's density at base_height; 0 for none
    extinction: vec3<f32>,    // mist's density's share per channel
    base_height: f32,         // world-Y every layer is measured from
    sun_color: vec3<f32>,     // sunlight reaching the fog, lux
    falloff: f32,             // 1/m mist's density drop per altitude (bigger = thinner, pools in valleys)
    sun_direction: vec3<f32>, // unit, toward the sun
    max_distance: f32,        // saturation distance; also the distance used for sky pixels
    haze_color: vec3<f32>,    // light the haze (molecules and aerosols) scatters toward the eye, cd/m²
    molecule_density: f32,    // molecules' density at base_height; 0 for none
    molecule_extinction: vec3<f32>, // molecules' density's share per channel
    molecule_falloff: f32,    // 1/m molecules' density drop per altitude
    start: f32,               // m from the eye before any fog
    sun_forward: f32,         // Henyey-Greenstein g of mist and aerosols: 0 scatters sunlight evenly, 1 straight on
    aerosol_density: f32,     // aerosols' density at base_height, every channel alike; 0 for none
    aerosol_falloff: f32,     // 1/m aerosols' density drop per altitude
    shadowed: u32,            // 1 with a fog shadow volume to read; 0 leaves the fog sunlit
}

// Whether any layer has density.
fn height_fog_on(fog: HeightFog) -> bool {
    return fog.density + fog.molecule_density + fog.aerosol_density > 0.0;
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

// Optical depth of the ray from cam_pos to world_pos past `start`: mist,
// molecules, aerosols. Mist pools low and the air's haze hangs at every
// altitude, its aerosols nearer the ground: no one exponential does all three.
fn height_fog_layers(fog: HeightFog, cam_pos: vec3<f32>, world_pos: vec3<f32>) -> vec3<f32> {
    let ray = world_pos - cam_pos;
    let d = length(ray);
    let dir_y = ray.y / max(d, 1e-4);
    let skipped = min(fog.start, d);
    let dist = min(d - skipped, fog.max_distance);
    let origin_y = cam_pos.y + dir_y * skipped;
    return layers_along(fog, origin_y, dist, dir_y);
}

// Each layer's optical depth along `dist` metres from `origin_y`, in its own
// units: multiply by its extinction for a channel's.
fn layers_along(fog: HeightFog, origin_y: f32, dist: f32, dir_y: f32) -> vec3<f32> {
    return vec3<f32>(
        layer_depth(fog.density, fog.falloff, fog.base_height, origin_y, dist, dir_y),
        layer_depth(fog.molecule_density, fog.molecule_falloff, fog.base_height, origin_y, dist, dir_y),
        layer_depth(fog.aerosol_density, fog.aerosol_falloff, fog.base_height, origin_y, dist, dir_y),
    );
}

// `layers`' optical depth in green, the channel visibility is quoted for.
fn green_depth(fog: HeightFog, layers: vec3<f32>) -> f32 {
    return layers.x * fog.extinction.y + layers.y * fog.molecule_extinction.y + layers.z;
}

// How much of what's behind the fog survives it, per channel.
fn height_fog_transmittance(fog: HeightFog, cam_pos: vec3<f32>, world_pos: vec3<f32>) -> vec3<f32> {
    let layers = height_fog_layers(fog, cam_pos, world_pos);
    return exp(-layers.x * fog.extinction - layers.y * fog.molecule_extinction - vec3<f32>(layers.z));
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

// The fog shadow volume (`fog_shadow.rs`): around the viewer, for each
// direction and distance, the share of the fog's light toward the eye that the
// terrain's shadow takes, summed from the fog's start out to that distance.
// World-aligned, so turning the head changes nothing in it, and both eyes read
// the one volume. x is the azimuth, wrapping; y the elevation, straight down to
// straight up, its rows closest together at the horizon; z the distance, as the
// root of its share of `shadow_volume_reach`, so slices are thin near the eye.
// A texel's distance is the same in every direction: were it a share of each
// ray's own length to the ground or the field's edge, neighbouring rows would
// hold different distances at one z, and filtering between them terraces.

// Share of the rows' spacing kept at the horizon, against a spacing even in
// the ray's height: smaller packs more rows there.
const VOLUME_HORIZON: f32 = 0.25;

// A direction's row, -1 straight down to 1 straight up.
fn shadow_volume_row(dir_y: f32) -> f32 {
    let a = VOLUME_HORIZON;
    let s = (sqrt(a * a + 4.0 * (1.0 - a) * abs(dir_y)) - a) / (2.0 * (1.0 - a));
    return sign(dir_y) * min(s, 1.0);
}

// `shadow_volume_row`'s inverse.
fn shadow_volume_dir_y(row: f32) -> f32 {
    return row * (VOLUME_HORIZON + (1.0 - VOLUME_HORIZON) * abs(row));
}

// The direction of the volume's cell (`column`, `row`) of `size`.
fn shadow_volume_direction(cell: vec2<u32>, size: vec2<u32>) -> vec3<f32> {
    let azimuth = ((f32(cell.x) + 0.5) / f32(size.x) - 0.5) * 6.2831853;
    let y = shadow_volume_dir_y(f32(cell.y) / f32(size.y - 1u) * 2.0 - 1.0);
    let flat = sqrt(max(1.0 - y * y, 0.0));
    return vec3<f32>(flat * cos(azimuth), y, flat * sin(azimuth));
}

// How far from the fog's start the volume reaches, the terrain's width: past
// it the fog counts as sunlit.
fn shadow_volume_reach(fog: HeightFog, params: SunShadowParams) -> f32 {
    return max(min(fog.max_distance, 1.0 / max(params.inv_world_size, 1e-9)), 1e-3);
}

// How far `world_pos` is above the terrain's shadow ceiling, in penumbra
// half-widths, as `sun_shadow_margin`, with a point under its column's ground
// taken at the surface: a lit column's ground is exactly at its ceiling, the
// penumbra's midpoint, and the lift takes it into full sun, as
// `ground_sun_visibility_at`. Large outside the field.
fn fog_shadow_margin(
    field: texture_2d<f32>,
    field_sampler: sampler,
    params: SunShadowParams,
    world_pos: vec3<f32>,
) -> f32 {
    let uv = world_pos.xz * params.inv_world_size + 0.5;
    if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) {
        return 1e3;
    }
    let field_sample = textureSampleLevel(field, field_sampler, uv, 0.0);
    let penumbra = max(field_sample.g * params.penumbra_per_metre, MIN_PENUMBRA);
    let height = max(world_pos.y - params.height_bias, field_sample.b + 2.0 * MIN_PENUMBRA);
    return (height - field_sample.r) / penumbra;
}

// The share of the fog's light between cam_pos and world_pos that the sun
// reaches, from the fog shadow volume, in green, the channel visibility is
// quoted for. The volume is centred on the viewer and read as if on cam_pos:
// an eye's few centimetres from it don't show in fog that starts metres away.
// `volume_sampler` must be linear and repeat in x. 1 with no volume: without
// one, the fog's parameters carry the viewer's own share of the sun
// (`EyeSunlight`).
fn fog_sunlit_share(
    fog: HeightFog,
    volume: texture_3d<f32>,
    volume_sampler: sampler,
    params: SunShadowParams,
    cam_pos: vec3<f32>,
    world_pos: vec3<f32>,
) -> f32 {
    if fog.shadowed == 0u || params.valid < 0.5 {
        return 1.0;
    }
    let ray = world_pos - cam_pos;
    let d = length(ray);
    let dir = ray / max(d, 1e-4);
    let skipped = min(fog.start, d);
    let dist = min(d - skipped, fog.max_distance);
    let start = cam_pos + dir * skipped;
    let total = 1.0 - exp(-green_depth(fog, layers_along(fog, start.y, dist, dir.y)));
    if total <= 1e-6 {
        return 1.0;
    }
    let reach = shadow_volume_reach(fog, params);
    let size = vec3<f32>(textureDimensions(volume));
    // Texel centres hold the first and last row and slice; the azimuth wraps.
    let at = vec3<f32>(
        atan2(dir.z, dir.x) / 6.2831853 + 0.5,
        (shadow_volume_row(dir.y) * 0.5 + 0.5) * (size.y - 1.0) / size.y + 0.5 / size.y,
        sqrt(min(dist / reach, 1.0)) * (size.z - 1.0) / size.z + 0.5 / size.z,
    );
    let shaded = textureSampleLevel(volume, volume_sampler, at, 0.0).r;
    return clamp(1.0 - shaded / total, 0.0, 1.0);
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
    let molecules = layers.y * fog.molecule_extinction;
    let aerosols = vec3<f32>(layers.z);
    let haze = molecules + aerosols;
    let depth = mist + haze;
    let transmittance = exp(-depth);
    // Each layer lends the ray its colour in proportion to the depth it adds.
    let mist_color = max(fog.color - (1.0 - sunlit) * fog.sun_color * EVEN_SHARE, vec3<f32>(0.0));
    let air = (mist * mist_color + haze * fog.haze_color) / max(depth, vec3<f32>(1e-30));
    // Mist and aerosols glow strongly toward the sun; molecules hardly.
    let cos_sun = dot(normalize(world_pos - cam_pos), fog.sun_direction);
    let forward = forward_glow(cos_sun, fog.sun_forward);
    let glow = ((mist + aerosols) * forward + molecules * molecular_glow(cos_sun)) / max(depth, vec3<f32>(1e-30));
    let scattered = air + fog.sun_color * glow * sunlit;
    return color * transmittance + scattered * exposure * (1.0 - transmittance);
}
