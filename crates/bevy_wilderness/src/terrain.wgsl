#import bevy_pbr::mesh_functions
#import bevy_pbr::view_transformations::position_world_to_clip

// Fragment-shading machinery, gated to the pipelines that shade (the forward
// fragment and the deferred g-buffer fragment). Depth-only pipelines — the
// shadow pass rendering the terrain's displaced silhouette for clay mode's
// dynamic shadows (D10), or a depth/normal prepass — compile this module too
// for `fn vertex`, but `pbr_fragment` doesn't build against `prepass_io`'s
// trimmed VertexOutput and their layouts lack the light/cluster bindings.
// `WILDERNESS_SHADE` is pushed in `GridMaterial::specialize`.
#ifdef WILDERNESS_SHADE
#import bevy_pbr::pbr_fragment::pbr_input_from_standard_material
#import bevy_pbr::mesh_view_bindings::{view, lights, clustered_lights}
#import bevy_pbr::{
    pbr_types,
    mesh_view_types,
    lighting,
    lighting::LAYER_BASE,
    clustered_forward as clustering,
    shadows,
    ambient,
    mesh_types::MESH_FLAGS_SHADOW_RECEIVER_BIT,
}

#ifdef ENVIRONMENT_MAP
#import bevy_pbr::environment_map
#endif

// Shared with the fullscreen fog post-process, so the VR (inline) and flatscreen
// (post-process) fog tiers use the exact same fog.
#import bevy_wilderness::fog_functions::{HeightFog, apply_height_fog}
#endif  // WILDERNESS_SHADE

#ifdef MESHLET_MESH_MATERIAL_PASS
#import bevy_pbr::meshlet_visibility_buffer_resolve::VertexOutput
#else ifdef PREPASS_PIPELINE
#import bevy_pbr::prepass_io::{Vertex, VertexOutput, FragmentOutput}
#ifdef DEFERRED_PREPASS
#import bevy_pbr::pbr_deferred_functions::deferred_output;
#endif  // DEFERRED_PREPASS
#else   // PREPASS_PIPELINE
#import bevy_pbr::forward_io::{Vertex, VertexOutput, FragmentOutput}
#import bevy_pbr::pbr_functions::main_pass_post_lighting_processing
#endif  // PREPASS_PIPELINE

@group(#{MATERIAL_BIND_GROUP}) @binding(102) var heightmap_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(103) var heightmap_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(108) var<uniform> texel_size: f32;
@group(#{MATERIAL_BIND_GROUP}) @binding(109) var<uniform> minmax: vec2<f32>;
// Packed flags (this material is at the bind-group binding limit, so quality
// knobs ride here instead of new uniforms): bit0 = wireframe, bit1 = ambient
// gather (sample RVT-AO), bit2 = single-layer detail.
@group(#{MATERIAL_BIND_GROUP}) @binding(111) var<uniform> flags: u32;
@group(#{MATERIAL_BIND_GROUP}) @binding(121) var rvt_albedo_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(122) var rvt_albedo_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(123) var rvt_normal_texture: texture_2d<f32>;
// Baked macro AO (R) + bent normal world X/Z (GB) + cavity (A).
@group(#{MATERIAL_BIND_GROUP}) @binding(132) var rvt_ao_texture: texture_2d<f32>;
// rvt_normal/rvt_ao share rvt_albedo_sampler (122) — all sampled linearly at uv.
// Dev/experiment scalars packed into one uniform (bindings 112/113 freed for
// ceiling headroom). ao/bent strength: 0 = off, 1 = full, each A/B'd on its own.
// debug_view: 0 = lit terrain, 1 = macro AO, 2 = bent normal, 3 = cavity —
// nonzero outputs the raw baked channel unlit.
struct DevParams {
    ao_strength: f32,
    bent_strength: f32,
    debug_view: u32,
}
@group(#{MATERIAL_BIND_GROUP}) @binding(110) var<uniform> dev: DevParams;
// Inline height fog params (VR tier). density == 0 skips it. Shading-only:
// the HeightFog type comes from the gated fog_functions import above.
#ifdef WILDERNESS_SHADE
@group(#{MATERIAL_BIND_GROUP}) @binding(114) var<uniform> fog: HeightFog;
#endif
@group(#{MATERIAL_BIND_GROUP}) @binding(125) var detail_albedo_array: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(126) var detail_albedo_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(127) var detail_normal_array: texture_2d_array<f32>;
// detail_normal/detail_orm share detail_albedo_sampler (126) — same tiling config.

// Near-range detail overlay parameters.
struct DetailParams {
    tiling: f32,
    normal_strength: f32,
    albedo_strength: f32,
    near: f32,
    far: f32,
}
@group(#{MATERIAL_BIND_GROUP}) @binding(129) var<uniform> detail: DetailParams;
@group(#{MATERIAL_BIND_GROUP}) @binding(130) var detail_orm_array: texture_2d_array<f32>;

// Editor visualization overlay (`editing` feature): 0..1 mask tinted into the
// surface albedo. Shares the heightmap sampler — no new sampler/uniform slots.
// The def is pushed by GridMaterial::specialize only in editing builds, so the
// declaration matches the bind-group layout on both sides.
#ifdef WILDERNESS_EDIT_OVERLAY
@group(#{MATERIAL_BIND_GROUP}) @binding(115) var edit_overlay_texture: texture_2d<f32>;
#endif

// Cheap per-point 2D hash in [0, 1)^2, seeded by world XZ. Used to stochastically
// jitter the NEAREST material-id read so its RVT-texel grid dithers into fine
// noise instead of a hard mosaic. Keyed on world position (not screen space) so
// the pattern is locked to the surface — no temporal sizzle under head motion,
// which matters here since there's no TAA to resolve it.
fn hash22(p_world: vec2<f32>) -> vec2<f32> {
    // Wrap to a 256 m tile first: `sin` on a raw far-from-origin coordinate loses
    // the sub-metre precision the hash needs (it would band or flatten out in the
    // distance). 256 m repeat is imperceptible at the ~5 cm cell size below.
    let p = p_world - floor(p_world * (1.0 / 256.0)) * 256.0;
    let r = vec2<f32>(dot(p, vec2<f32>(127.1, 311.7)), dot(p, vec2<f32>(269.5, 183.3)));
    return fract(sin(r) * 43758.5453);
}

// Fetch one heightmap texel, resolving out-of-range indices two ways: looping
// (flags bit3) wraps toroidally so the far rings read the tiled heightmap;
// otherwise clamp to the edge (finite terrain — uv == 1.0 mustn't read OOB).
fn load_height_texel(p: vec2<i32>, size: vec2<i32>, lod: i32) -> f32 {
    var idx = clamp(p, vec2(0), size - 1);
    if (flags & 8u) != 0u {
        idx = ((p % size) + size) % size;
    }
    return textureLoad(heightmap_texture, idx, lod).r;
}

fn height_bilinear(uv: vec2<f32>, lod: i32) -> f32 {
    let tex_size = vec2<i32>(textureDimensions(heightmap_texture, lod));
    let pos = uv * vec2<f32>(tex_size);
    let p0 = vec2<i32>(floor(pos));
    let f = pos - floor(pos);

    let h00 = load_height_texel(p0, tex_size, lod);
    let h10 = load_height_texel(p0 + vec2(1, 0), tex_size, lod);
    let h01 = load_height_texel(p0 + vec2(0, 1), tex_size, lod);
    let h11 = load_height_texel(p0 + vec2(1, 1), tex_size, lod);

    let hx0 = mix(h00, h10, f.x);
    let hx1 = mix(h01, h11, f.x);

    return mix(hx0, hx1, f.y);
}

@vertex
fn vertex(vertex: Vertex, @builtin(vertex_index) idx: u32) -> VertexOutput {
    var out: VertexOutput;
    let model = mesh_functions::get_world_from_local(vertex.instance_index);
    out.world_position = model * vec4<f32>(vertex.position, 1.0);

    let texture_size = vec2<f32>(textureDimensions(heightmap_texture));
    let world_size = texel_size * texture_size;

    let looping = (flags & 8u) != 0u;
    let height_uv = out.world_position.xz / world_size + 0.5;
    // Looping wraps the read toroidally (height_bilinear handles the indices), so
    // the uv isn't clamped; finite terrain clamps to keep the far edge in bounds.
    let sample_uv = select(clamp(height_uv, vec2(0.0), vec2(1.0)), height_uv, looping);
    let height = height_bilinear(sample_uv, 0);
    let world_y = height * (minmax.y - minmax.x) + minmax.x;

    // Out past the heightmap coverage the coarse LOD skirt would render as a wall
    // at the edge height. Drop those vertices to the height floor so the stripe
    // stays low and out of sight (the sample above is clamped, not read OOB).
    // When looping there is no "outside" — the tiled heightmap covers every ring.
    let in_coverage = all(height_uv >= vec2(0.0)) && all(height_uv <= vec2(1.0));
    out.world_position.y = select(minmax.x, world_y, in_coverage || looping);
    out.position = position_world_to_clip(out.world_position.xyz);

    return out;
}

// Octahedral decode of a 0..1 encoded unit vector (2 channels).
fn oct_decode(f: vec2<f32>) -> vec3<f32> {
    let e = f * 2.0 - 1.0;
    let n = vec3<f32>(e.x, e.y, 1.0 - abs(e.x) - abs(e.y));
    let t = max(-n.z, 0.0);
    let xy = n.xy + select(vec2(t), vec2(-t), n.xy >= vec2(0.0));
    return normalize(vec3<f32>(xy, n.z));
}

// Everything below is fragment shading — compiled only for the pipelines that
// shade (see the WILDERNESS_SHADE import note at the top). Depth-only
// pipelines end at `fn vertex` above.
#ifdef WILDERNESS_SHADE

// Terrain PBR lighting with baked sun-visibility injected on the directional sun.
// Compact fork of `apply_pbr_lighting`: base layer only, directional + clustered
// point/spot lights + ambient + environment map (no clearcoat / transmission /
// anisotropy — the terrain doesn't use them). `sun_vis` (0..1) attenuates only the
// direct sun, so shadowed valleys still receive sky/ambient light; point and spot
// lights are unaffected by it (it's the sun's baked occlusion, not theirs).
// Filament GTAO multi-bounce: turns a scalar visibility into a colored occlusion
// that approximates light bouncing back out of the cavity, so occluded albedo
// stays saturated (warm rock stays warm) instead of darkening toward gray.
fn gtao_multi_bounce(visibility: f32, albedo: vec3<f32>) -> vec3<f32> {
    let a = 2.0404 * albedo - vec3<f32>(0.3324);
    let b = -4.7951 * albedo + vec3<f32>(0.6417);
    let c = 2.7552 * albedo + vec3<f32>(0.6903);
    return max(vec3<f32>(visibility), ((visibility * a + b) * visibility + c) * visibility);
}

// Filament specular occlusion: derive how much the specular (sky reflection) lobe
// is occluded from the diffuse AO, tightened by roughness and view angle. Keeps
// valley/crevice sky reflections from staying bright where the diffuse is dark.
fn specular_occlusion_from_ao(n_dot_v: f32, ao: f32, roughness: f32) -> f32 {
    return saturate(pow(n_dot_v + ao, exp2(-16.0 * roughness - 1.0)) - 1.0 + ao);
}

// `bent_N` is the baked average-unoccluded direction; it feeds only the analytic
// ambient (the sky/diffuse term), so a valley floor draws sky light from the strip
// it can actually see rather than the whole hemisphere. Direct/specular still use
// the shading normal `in.N`.
fn terrain_apply_lighting(in: pbr_types::PbrInput, sun_vis: f32, bent_N: vec3<f32>) -> vec4<f32> {
    let base_color = in.material.base_color;
    let metallic = in.material.metallic;
    let perceptual_roughness = in.material.perceptual_roughness;
    let roughness = lighting::perceptualRoughnessToRoughness(perceptual_roughness);
    let reflectance = in.material.reflectance;
    let diffuse_color = base_color.rgb * (1.0 - metallic);
    let NdotV = max(dot(in.N, in.V), 0.0001);
    let R = reflect(-in.V, in.N);
    let F_ab = lighting::F_AB(perceptual_roughness, NdotV);
    let F0 = 0.16 * reflectance * reflectance * (1.0 - metallic) + base_color.rgb * metallic;

    var li: lighting::LightingInput;
    li.layers[LAYER_BASE].NdotV = NdotV;
    li.layers[LAYER_BASE].N = in.N;
    li.layers[LAYER_BASE].R = R;
    li.layers[LAYER_BASE].perceptual_roughness = perceptual_roughness;
    li.layers[LAYER_BASE].roughness = roughness;
    li.P = in.world_position.xyz;
    li.V = in.V;
    li.diffuse_color = diffuse_color;
    li.metallic = metallic;
    li.F0_dielectric = 0.16 * reflectance * reflectance;
    li.F0_metallic = base_color.rgb;
    li.F_ab = F_ab;

    let view_z = dot(vec4<f32>(
        view.view_from_world[0].z,
        view.view_from_world[1].z,
        view.view_from_world[2].z,
        view.view_from_world[3].z,
    ), in.world_position);

    // Suppress the sun's grazing specular on rough terrain. Bevy's BRDF assumes
    // full grazing Fresnel (f90 ≈ 1); dry rough ground doesn't reach it (its
    // microgeometry self-shadows at grazing), so a low sun otherwise sheens the
    // whole sunward slope white — dry terrain looks wet at sunset. Keep only
    // `SUN_SPEC_KEEP` of the specular when fully rough, ~all of it when smooth
    // (wet rock / ice / water keep their sheen). `directional_light` with diffuse
    // off is the specular-only lobe, subtracted below so diffuse/shadow/atmosphere
    // stay intact.
    const SUN_SPEC_KEEP = 0.25;
    let sun_spec_keep = mix(1.0, SUN_SPEC_KEEP, perceptual_roughness);

    // Directional lights (the sun), attenuated by CSM shadow × baked sun-visibility.
    var direct = vec3<f32>(0.0);
    let n_dir = lights.n_directional_lights;
    for (var i = 0u; i < n_dir; i++) {
        var shadow = 1.0;
        if ((in.flags & MESH_FLAGS_SHADOW_RECEIVER_BIT) != 0u
            && (lights.directional_lights[i].flags & mesh_view_types::DIRECTIONAL_LIGHT_FLAGS_SHADOWS_ENABLED_BIT) != 0u) {
            shadow = shadows::fetch_directional_shadow(i, in.world_position, in.world_normal, view_z, in.frag_coord.xy);
        }
        var contrib = lighting::directional_light(i, &li, true);
        if sun_spec_keep < 0.999 {
            contrib -= (1.0 - sun_spec_keep) * lighting::directional_light(i, &li, false);
        }
        direct += contrib * shadow * sun_vis;
    }

    // Clusterable lights (point + spot) touching this fragment's cluster. The same
    // `ranges` also feeds the environment-map lookup below.
    let cluster_index = clustering::view_fragment_cluster_index(in.frag_coord.xy, view_z, in.is_orthographic);
    var ranges = clustering::unpack_clusterable_object_index_ranges(cluster_index);

    // Point lights, each attenuated by its own shadow map (if enabled). Not touched
    // by sun_vis — the baked sun occlusion doesn't occlude local lights.
    for (var i = ranges.first_point_light_index_offset; i < ranges.first_spot_light_index_offset; i++) {
        let light_id = clustering::get_clusterable_object_id(i);
        var shadow = 1.0;
        if ((in.flags & MESH_FLAGS_SHADOW_RECEIVER_BIT) != 0u
            && (clustered_lights.data[light_id].flags & mesh_view_types::POINT_LIGHT_FLAGS_SHADOWS_ENABLED_BIT) != 0u) {
            shadow = shadows::fetch_point_shadow(light_id, in.world_position, in.world_normal, in.frag_coord.xy);
        }
        direct += lighting::point_light(light_id, &li, true, true) * shadow;
    }

    // Spot lights, likewise shadowed per-light.
    for (var i = ranges.first_spot_light_index_offset; i < ranges.first_reflection_probe_index_offset; i++) {
        let light_id = clustering::get_clusterable_object_id(i);
        var shadow = 1.0;
        if ((in.flags & MESH_FLAGS_SHADOW_RECEIVER_BIT) != 0u
            && (clustered_lights.data[light_id].flags & mesh_view_types::POINT_LIGHT_FLAGS_SHADOWS_ENABLED_BIT) != 0u) {
            shadow = shadows::fetch_spot_shadow(
                light_id,
                in.world_position,
                in.world_normal,
                clustered_lights.data[light_id].shadow_map_near_z,
                in.frag_coord.xy,
            );
        }
        direct += lighting::spot_light(light_id, &li, true) * shadow;
    }

    // Indirect: environment map (the physical-sky IBL) + flat ambient.
    var indirect = vec3<f32>(0.0);
#ifdef ENVIRONMENT_MAP
    // The bent normal drives the diffuse irradiance sample direction: an occluded
    // point (a valley floor) gathers sky only from the direction it can actually
    // see, instead of the full hemisphere about its geometric normal. Free — the
    // env map is already sampled; this only changes the lookup direction, and it
    // is the whole reason the bent normal is baked. `bent_N` == `in.N` when the
    // effect is toggled off, so this reduces to the original behavior.
    li.layers[LAYER_BASE].N = bent_N;
    let env = environment_map::environment_map_light(&li, &ranges, false);
    // Colored multi-bounce diffuse occlusion + AO-derived specular occlusion, so
    // the sky IBL is occluded physically rather than uniformly darkened.
    let ao_scalar = in.diffuse_occlusion.r;
    let diffuse_occ = gtao_multi_bounce(ao_scalar, diffuse_color);
    let spec_occ = specular_occlusion_from_ao(NdotV, ao_scalar, roughness);
    indirect += env.diffuse * diffuse_occ + env.specular * spec_occ;
#endif
    // Flat ambient ignores the normal entirely; keep it on the shading normal.
    indirect += ambient::ambient_light(in.world_position, in.N, in.V, NdotV, diffuse_color, F0, perceptual_roughness, in.diffuse_occlusion);

    let emissive = in.material.emissive.rgb * base_color.a;
    var color = view.exposure * (direct + indirect) + emissive;
    // Inline height fog (VR tier); no-op when density == 0 (flatscreen uses the
    // fullscreen post-process instead). Terrain-only but virtually free.
    if fog.density > 0.0 {
        color = apply_height_fog(fog, color, view.world_position, in.world_position.xyz);
    }
    return vec4<f32>(color, base_color.a);
}

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    if (flags & 1u) != 0u {
        var out: FragmentOutput;
        out.color = vec4(1.0);
        return out;
    }

    var in_modified = in;

    let texture_size = vec2<f32>(textureDimensions(heightmap_texture));
    let world_size = texture_size * texel_size;
    let uv = in.world_position.xz / world_size + 0.5;

    // Clay display mode (flags bit4, D10): shading is stale — unbaked edits,
    // or no bake has landed yet — so ignore the baked RVT entirely and render
    // neutral grey clay lit by the real lights, with normals from position
    // derivatives (faceted, deliberately: it reads as sculpting clay and
    // needs nothing baked). The mask overlay stays visible — masking is part
    // of the modeling session clay exists for.
    if (flags & 16u) != 0u {
        var clay_in = in;
        var n = normalize(cross(dpdy(in.world_position.xyz), dpdx(in.world_position.xyz)));
        n = select(n, -n, n.y < 0.0);
        clay_in.world_normal = n;
        var albedo = vec3<f32>(0.5);
#ifdef WILDERNESS_EDIT_OVERLAY
        let clay_overlay_uv = select(clamp(uv, vec2(0.0), vec2(1.0)), fract(uv), (flags & 8u) != 0u);
        let clay_overlay = textureSample(edit_overlay_texture, heightmap_sampler, clay_overlay_uv).r;
        albedo = mix(albedo, vec3<f32>(1.0, 0.25, 0.05), clay_overlay * 0.5);
#endif
        var clay_pbr = pbr_input_from_standard_material(clay_in, is_front);
        clay_pbr.material.base_color = vec4<f32>(albedo, 1.0);
        clay_pbr.material.perceptual_roughness = 0.95;
        clay_pbr.material.metallic = 0.0;
        clay_pbr.diffuse_occlusion = vec3<f32>(1.0);
#ifdef PREPASS_PIPELINE
        let out = deferred_output(clay_in, clay_pbr);
#else
        var out: FragmentOutput;
        // Sun visibility 1.0: there's no valid bake to shadow with.
        out.color = terrain_apply_lighting(clay_pbr, 1.0, n);
        out.color = main_pass_post_lighting_processing(clay_pbr, out.color);
#endif
        return out;
    }

    // Sample the baked RVT instead of blending the splat per-fragment:
    // albedo + baked sun-visibility (alpha); octahedral world normal + roughness
    // + packed material ids (alpha, read NEAREST below).
    let rvt_a = textureSample(rvt_albedo_texture, rvt_albedo_sampler, uv);
    let rvt_n = textureSample(rvt_normal_texture, rvt_albedo_sampler, uv);
    let base_normal = oct_decode(rvt_n.rg);
    // Macro AO (R) + bent normal world X/Z (GB, Y reconstructed) + cavity (A).
    // Skipped when the ambient gather is disabled (VR): the sample and the RVT-AO
    // target are dropped, falling back to unoccluded ambient + the geometric normal.
    var rvt_ao_s = vec4<f32>(1.0, 0.5, 0.5, 0.5);
    var macro_ao = 1.0;
    var baked_bent_normal = base_normal;
    var cavity = 0.5;
    if (flags & 2u) != 0u {
        rvt_ao_s = textureSample(rvt_ao_texture, rvt_albedo_sampler, uv);
        macro_ao = mix(1.0, rvt_ao_s.r, dev.ao_strength);
        let bent_xz = rvt_ao_s.gb * 2.0 - 1.0;
        let bent_y = sqrt(max(0.0, 1.0 - dot(bent_xz, bent_xz)));
        baked_bent_normal = vec3<f32>(bent_xz.x, bent_y, bent_xz.y);
        cavity = rvt_ao_s.a;
    }

    let cam_dist = distance(view.world_position, in.world_position.xyz);

    // Near-range detail overlay, faded with distance. Skipped entirely past the
    // fade range so far terrain pays none of the detail samples. Derivatives are
    // taken outside the branch so the guarded samples keep correct mip selection.
    let detail_fade = 1.0 - smoothstep(detail.near, detail.far, cam_dist);
    let dtile = in.world_position.xz / detail.tiling;
    let ddx = dpdx(dtile);
    let ddy = dpdy(dtile);

    var world_normal = base_normal;
    var albedo = rvt_a.rgb;
    var rough = rvt_n.b;
    var ao = 1.0;
    if detail_fade > 0.001 {
        // Two dominant material ids + their blend, packed (2+2+4 bits) into the
        // RVT's metallic slot. `mblend` (0..0.5) lerps the two materials' detail
        // normals. Read it NEAREST (textureLoad) — the packed byte can't be
        // linearly filtered, or the bilinear sweep through id/weight combos shows
        // as banding strips. Only the detail branch consumes it, so the load (a
        // guaranteed full-res cache miss at distance) is paid only near the camera.
        let rvt_dims = vec2<f32>(textureDimensions(rvt_normal_texture));
        // Jitter the sample point by ±0.5 texel using a world-locked hash before
        // the NEAREST read. Rounding a coordinate offset by uniform noise picks
        // each neighbouring texel with probability equal to the fractional
        // distance — a stochastic stand-in for the bilinear filter the packed
        // byte can't use. The hard dirt→rock boundary becomes noise the detail
        // texture hides. The high-frequency hash keeps cells sub-texel so
        // adjacent fragments decorrelate and average toward the true blend.
        let jitter = hash22(in.world_position.xz) - 0.5;
        // Wrap the NEAREST read toroidally when looping so the packed material id
        // has no seam at the tile edge; otherwise clamp to the RVT bounds.
        let rvt_idim = vec2<i32>(rvt_dims);
        let mid_raw = vec2<i32>(floor(uv * rvt_dims + jitter));
        var mid_idx = clamp(mid_raw, vec2(0), rvt_idim - 1);
        if (flags & 8u) != 0u {
            mid_idx = ((mid_raw % rvt_idim) + rvt_idim) % rvt_idim;
        }
        let mid = u32(textureLoad(rvt_normal_texture, mid_idx, 0).a * 255.0 + 0.5);
        let id0 = mid & 3u;
        let id1 = (mid >> 2u) & 3u;
        let mblend = f32((mid >> 4u) & 15u) / 15.0 * 0.5;

        // Top dominant material's detail normal / albedo / ORM. The second material
        // is lerped in for smooth boundaries — skipped for single-layer detail (VR)
        // and wherever one material dominates (`mblend == 0`, the common case, which
        // the bake quantizes exactly to 0), saving three `textureSampleGrad`s.
        var dn = textureSampleGrad(detail_normal_array, detail_albedo_sampler, dtile, id0, ddx, ddy).xyz * 2.0 - 1.0;
        var da = textureSampleGrad(detail_albedo_array, detail_albedo_sampler, dtile, id0, ddx, ddy).rgb;
        var dorm = textureSampleGrad(detail_orm_array, detail_albedo_sampler, dtile, id0, ddx, ddy);
        if (flags & 4u) == 0u && mblend > 0.0 {
            let dn1 = textureSampleGrad(detail_normal_array, detail_albedo_sampler, dtile, id1, ddx, ddy).xyz * 2.0 - 1.0;
            let da1 = textureSampleGrad(detail_albedo_array, detail_albedo_sampler, dtile, id1, ddx, ddy).rgb;
            let dorm1 = textureSampleGrad(detail_orm_array, detail_albedo_sampler, dtile, id1, ddx, ddy);
            dn = mix(dn, dn1, mblend);
            da = mix(da, da1, mblend);
            dorm = mix(dorm, dorm1, mblend);
        }
        // Flip X (see bake) and reorient onto the base normal.
        dn = dn * vec3<f32>(-1.0, 1.0, 1.0);
        let dn_scaled = vec3<f32>(dn.xy * detail.normal_strength * detail_fade, dn.z);
        let ref_axis = select(vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(1.0, 0.0, 0.0), abs(base_normal.z) > 0.99);
        let dt = normalize(cross(ref_axis, base_normal));
        let db = cross(base_normal, dt);
        world_normal = normalize(dt * dn_scaled.x + db * dn_scaled.y + base_normal * dn_scaled.z);

        albedo *= mix(vec3<f32>(1.0), 2.0 * da, detail.albedo_strength * detail_fade);
        rough = mix(rvt_n.b, dorm.g, detail_fade);
        ao = mix(1.0, dorm.r, detail_fade);
    }
    in_modified.world_normal = world_normal;

#ifdef WILDERNESS_EDIT_OVERLAY
    // Editor mask visualization: tint the albedo where the overlay is painted.
    // Applied to albedo (not post-lighting) so it shades plausibly and works in
    // the deferred path too. `fract` wraps the sample on looping terrain.
    let overlay_uv = select(clamp(uv, vec2(0.0), vec2(1.0)), fract(uv), (flags & 8u) != 0u);
    let overlay = textureSample(edit_overlay_texture, heightmap_sampler, overlay_uv).r;
    albedo = mix(albedo, vec3<f32>(1.0, 0.25, 0.05), overlay * 0.5);
#endif

    var pbr_input = pbr_input_from_standard_material(in_modified, is_front);
    pbr_input.material.base_color = vec4<f32>(albedo, 1.0);
    pbr_input.material.perceptual_roughness = rough;
    pbr_input.material.metallic = 0.0;
    // Macro AO folds into diffuse occlusion, so it darkens only ambient/indirect
    // (never the direct sun). Bent normal blends in from the shading normal by its
    // own strength; with both strengths 0 this reproduces the pre-experiment look.
    pbr_input.diffuse_occlusion = vec3<f32>(ao * macro_ao);
    let bent_normal = normalize(mix(world_normal, baked_bent_normal, dev.bent_strength));

#ifdef PREPASS_PIPELINE
    let out = deferred_output(in_modified, pbr_input);
#else
    var out: FragmentOutput;
    // Debug: output a single baked channel unlit so the tonemapper shows it as a
    // literal value (grayscale for AO/cavity, encoded RGB for the bent normal).
    if dev.debug_view == 1u {
        out.color = vec4<f32>(vec3<f32>(rvt_ao_s.r), 1.0);
    } else if dev.debug_view == 2u {
        out.color = vec4<f32>(baked_bent_normal * 0.5 + 0.5, 1.0);
    } else if dev.debug_view == 3u {
        out.color = vec4<f32>(vec3<f32>(rvt_ao_s.a), 1.0);
    } else {
        out.color = terrain_apply_lighting(pbr_input, rvt_a.a, bent_normal);
        out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    }
#endif

    return out;
}

#endif  // WILDERNESS_SHADE
