// Proves a bake reached the shadow field: each texel of a tiny target reads the
// field under it and is white only where the field holds the current bake.
import bevy_pbr::render::forward_io::VertexOutput;

@group(constants::MATERIAL_BIND_GROUP) @binding(0) var field: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var<uniform> stamp: f32;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let size = vec2<f32>(textureDimensions(field));
    let texel = vec2<i32>(clamp(in.uv * size, vec2(0.0), size - 1.0));
    let sample = textureLoad(field, texel, 0);
    // `bake.wesl` keeps R at 1 or more; the clear leaves it 0.
    let drawn = sample.r >= 0.5 && abs(sample.a - stamp) < 0.5;
    return vec4<f32>(select(0.0, 1.0, drawn), 0.0, 0.0, 1.0);
}
