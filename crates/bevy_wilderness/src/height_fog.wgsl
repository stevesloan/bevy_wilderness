// Fullscreen height fog (flatscreen tier): fogs the whole view — terrain and
// atmosphere sky — so valley mist has no terrain/sky seam. Reconstructs world
// position from depth and applies the shared fog math.
#import bevy_core_pipeline::fullscreen_vertex_shader::FullscreenVertexOutput
#import bevy_render::view::View
#import bevy_wilderness::fog_functions::{HeightFog, apply_height_fog}

@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var depth_texture: texture_2d<f32>;
@group(0) @binding(2) var screen_texture: texture_2d<f32>;
@group(0) @binding(3) var screen_sampler: sampler;
@group(0) @binding(4) var<uniform> fog: HeightFog;

@fragment
fn fragment(in: FullscreenVertexOutput) -> @location(0) vec4<f32> {
    let scene = textureSample(screen_texture, screen_sampler, in.uv);
    let raw_depth = textureLoad(depth_texture, vec2<i32>(in.position.xy), 0).x;

    let ndc_xy = vec2<f32>(in.uv.x * 2.0 - 1.0, 1.0 - in.uv.y * 2.0); // uv y-down, ndc y-up

    var world_pos: vec3<f32>;
    if raw_depth <= 0.0 {
        // Sky (reverse-Z far): a far point along the view ray, so fog saturates.
        let p_near_h = view.world_from_clip * vec4<f32>(ndc_xy, 1.0, 1.0);
        let p_far_h = view.world_from_clip * vec4<f32>(ndc_xy, 0.0001, 1.0);
        let dir = normalize(p_far_h.xyz / p_far_h.w - p_near_h.xyz / p_near_h.w);
        world_pos = view.world_position + dir * fog.max_distance;
    } else {
        let wp_h = view.world_from_clip * vec4<f32>(ndc_xy, raw_depth, 1.0);
        world_pos = wp_h.xyz / wp_h.w;
    }

    let fogged = apply_height_fog(fog, scene.rgb, view.world_position, world_pos);
    return vec4<f32>(fogged, scene.a);
}
