// StandardMaterial + inline height fog for meshes, matching the terrain fog on the
// `Low` tier. Shared fog math; no-op when density == 0 (the `High` tier, where the
// fullscreen pass fogs meshes by depth).
#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
    forward_io::{VertexOutput, FragmentOutput},
    mesh_view_bindings::view,
}
#import bevy_wilderness::fog_functions::{HeightFog, apply_height_fog}

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<uniform> fog: HeightFog;

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);
    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);
    if fog.density > 0.0 {
        out.color = vec4<f32>(
            apply_height_fog(fog, out.color.rgb, view.world_position, in.world_position.xyz),
            out.color.a,
        );
    }
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
