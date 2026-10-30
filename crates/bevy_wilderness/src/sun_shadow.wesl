// Sampling for the baked sun shadow-ceiling field (see `shadow_ceiling` in
// `bake.wesl`). Imported by the terrain and by *game* materials that want the
// same terrain shadow on their own meshes — characters, vehicles, dropped
// pickups. The clipmap is typically `NotShadowCaster` (rendering it into the
// cascades costs more than it returns, and doubly so in stereo), so cascaded
// shadow maps never darken an object standing behind a mountain; this does.
//
// The field is a plain 2D texture, so this costs one fetch and works at any
// height — a flying player resolves correctly, which an on-surface shadow map
// cannot do.
struct SunShadowParams {
    // World XZ -> UV: `uv = world_xz * inv_world_size + 0.5`. The field covers
    // the clipmap, which is origin-centered.
    inv_world_size: f32,
    // Added back to the R channel, which is stored biased so it is never zero.
    height_bias: f32,
    // Penumbra half-width per metre of occluder distance. The sun is a ~0.53°
    // disc, so its shadow edge widens with distance to the caster — a ridge
    // 2 km away has a penumbra tens of metres wide, and holding that edge sharp
    // is what makes baked terrain shadows look painted on.
    penumbra_per_metre: f32,
    // 0 while the field has not been baked, 1 once it has. Unbaked reads as full
    // sun rather than a black world.
    valid: f32,
}

// 1.0 = full sun, 0.0 = fully shadowed by terrain, soft between.
fn sun_visibility(
    field: texture_2d<f32>,
    field_sampler: sampler,
    params: SunShadowParams,
    world_pos: vec3<f32>,
) -> f32 {
    if params.valid < 0.5 {
        return 1.0;
    }
    let uv = world_pos.xz * params.inv_world_size + 0.5;
    // Outside the terrain nothing is baked to occlude with; the clamped edge
    // texel would otherwise smear the border ridge across the whole horizon.
    if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) {
        return 1.0;
    }
    return sun_visibility_at(field, field_sampler, params, uv, world_pos.y);
}

// `sun_visibility` at field coordinate `uv` and world height `height`, for a
// caller that maps world to field itself (looping terrain wraps it). Assumes
// `params.valid`.
fn sun_visibility_at(
    field: texture_2d<f32>,
    field_sampler: sampler,
    params: SunShadowParams,
    uv: vec2<f32>,
    height: f32,
) -> f32 {
    let field_sample = textureSampleLevel(field, field_sampler, uv, 0.0);
    let ceiling = field_sample.r + params.height_bias;
    let occluder_distance = field_sample.g;
    // Bilinear across a texel already softens the edge; the penumbra widens it to
    // the physical size, and the `max` keeps a near-field occluder from
    // collapsing to a hard step.
    let penumbra = max(occluder_distance * params.penumbra_per_metre, MIN_PENUMBRA);
    return smoothstep(-penumbra, penumbra, height - ceiling);
}

// Narrowest penumbra half-width (m). A point this far above the ceiling is in
// full sun.
const MIN_PENUMBRA: f32 = 0.25;
