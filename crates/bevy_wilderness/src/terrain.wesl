import bevy_pbr::render::mesh_functions;
import bevy_pbr::render::view_transformations::position_world_to_clip;
import bevy_pbr::render::mesh_view_bindings::view;
import bevy_wilderness::sun_shadow::{SunShadowParams, sun_visibility_at, MIN_PENUMBRA};

// Fragment-shading machinery, gated to the pipelines that shade (the forward
// fragment and the deferred g-buffer fragment). Depth-only pipelines — the
// shadow pass rendering the terrain's displaced silhouette for clay mode's
// dynamic shadows, or a depth/normal prepass — compile this module too
// for `fn vertex`, but `pbr_fragment` doesn't build against `prepass::io`'s
// trimmed VertexOutput and their layouts lack the light/cluster bindings.
// `WILDERNESS_SHADE` is pushed in `GridMaterial::specialize`.
@if(WILDERNESS_SHADE)
import bevy_pbr::render::{
    pbr_fragment::pbr_input_from_standard_material,
    mesh_view_bindings::{lights, clustered_lights},
    pbr_types,
    mesh_view_types,
    pbr_lighting as lighting,
    pbr_lighting::LAYER_BASE,
    clustered_forward as clustering,
    shadows,
    pbr_ambient as ambient,
    mesh_types::MESH_FLAGS_SHADOW_RECEIVER_BIT,
};

@if(WILDERNESS_SHADE && ENVIRONMENT_MAP)
import bevy_pbr::light_probe::environment_map;

// Shared with every other material that fogs itself.
@if(WILDERNESS_SHADE)
import bevy_wilderness::fog_functions::{HeightFog, apply_height_fog};

@if(MESHLET_MESH_MATERIAL_PASS)
import bevy_pbr::meshlet::visibility_buffer_resolve::VertexOutput;
@elif(PREPASS_PIPELINE)
import bevy_pbr::prepass::io::{Vertex, VertexOutput, FragmentOutput};
@else
import bevy_pbr::render::{
    forward_io::{Vertex, VertexOutput, FragmentOutput},
    pbr_functions::main_pass_post_lighting_processing,
};

@if(!MESHLET_MESH_MATERIAL_PASS && PREPASS_PIPELINE && DEFERRED_PREPASS)
import bevy_pbr::deferred::functions::deferred_output;

@group(constants::MATERIAL_BIND_GROUP) @binding(102) var heightmap_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(103) var heightmap_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(108) var<uniform> texel_size: f32;
@group(constants::MATERIAL_BIND_GROUP) @binding(109) var<uniform> minmax: vec2<f32>;
// Packed flags (this material is at the bind-group binding limit, so quality
// knobs ride here instead of new uniforms): bit0 = wireframe, bit1 unused,
// bit2 = single-layer detail, bit3 = looping.
@group(constants::MATERIAL_BIND_GROUP) @binding(111) var<uniform> flags: u32;
@group(constants::MATERIAL_BIND_GROUP) @binding(121) var rvt_albedo_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(122) var rvt_albedo_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(123) var rvt_normal_texture: texture_2d<f32>;
// The sun shadow-ceiling field (see `sun_shadow.wesl`), which shadows the
// ground as it does meshes. Its params ride in `dev.sun_shadow`.
@group(constants::MATERIAL_BIND_GROUP) @binding(132) var sun_shadow_field: texture_2d<f32>;
// The host's baked lighting (see `baked_light.rs`), sampled with 122. Its
// regions ride in `dev.baked`; with none, nothing reads it.
@group(constants::MATERIAL_BIND_GROUP) @binding(133) var baked_light_atlas: texture_2d<f32>;
// Matches `BakedRegionGpu`: world = min xz, max xz; color = atlas UV min, max;
// direction.xy = its tile's UV min, the same size as `color`'s.
struct BakedRegion {
    world: vec4<f32>,
    color: vec4<f32>,
    direction: vec4<f32>,
}
// Matches `BakedLightParams`.
struct BakedLight {
    count: u32,
    exposure: f32,
    sky_luminance: f32,
    patch_fade: f32,
    regions: array<BakedRegion, 8>,
}
// rvt_normal and the field share rvt_albedo_sampler (122), all sampled linearly.
// Dev/experiment + editing scalars packed into one uniform (bindings 112/113
// freed for ceiling headroom). debug_view: 0 = lit terrain, 1 = cavity,
// 2 = sun visibility — nonzero outputs the raw value unlit. The stamp_*
// fields are the stamp preview transform, read in `fn vertex` while flags
// bit5 is set (must match the Rust `DevParams` field-for-field).
struct DevParams {
    sun_shadow: SunShadowParams,
    baked: BakedLight,
    debug_view: u32,
    stamp_center: vec2<f32>,
    stamp_half_size: vec2<f32>,
    stamp_rotation: f32,
    stamp_strength: f32,
    holes: HoleBuffer,
    hole_count: u32,
    // Clipmap half-width in cells: each ring spans half_width × its cell size
    // from its center. Drives the geomorph band.
    ring_half_cells: f32,
}
// Terrain holes, two vec4 per shape (packed by Rust `HoleBuffer::pack`):
//   [0] = center.xy, basis.xy      (basis = rect local +X; unused for circles)
//   [1] = half.xy | radius, kind (0 rect / 1 circle), min_extent
struct HoleBuffer {
    shapes: array<vec4<f32>, 48>,
}
// A shape only cuts a LOD ring whose cells it spans at least this many times
// over, so small holes seal at distance rather than removing huge far
// triangles. Tune in-headset; 3 bounds the visible hole to ~1.7× authored.
const HOLE_MIN_CELLS: f32 = 3.0;
@group(constants::MATERIAL_BIND_GROUP) @binding(110) var<uniform> dev: DevParams;
// Height fog params. density == 0 skips it. Shading-only:
// the HeightFog type comes from the gated fog_functions import above.
@if(WILDERNESS_SHADE)
@group(constants::MATERIAL_BIND_GROUP) @binding(114) var<uniform> fog: HeightFog;
@group(constants::MATERIAL_BIND_GROUP) @binding(125) var detail_albedo_array: texture_2d_array<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(126) var detail_albedo_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(127) var detail_normal_array: texture_2d_array<f32>;
// detail_normal/detail_orm share detail_albedo_sampler (126) — same tiling config.

// Near-range detail overlay parameters.
struct DetailParams {
    tiling: f32,
    normal_strength: f32,
    albedo_strength: f32,
    near: f32,
    far: f32,
}
@group(constants::MATERIAL_BIND_GROUP) @binding(129) var<uniform> detail: DetailParams;
@group(constants::MATERIAL_BIND_GROUP) @binding(130) var detail_orm_array: texture_2d_array<f32>;

// Editor visualization overlay (`editing` feature): 0..1 mask tinted into the
// surface albedo. Shares the heightmap sampler — no new sampler/uniform slots.
// The def is pushed by GridMaterial::specialize only in editing builds, so the
// declaration matches the bind-group layout on both sides.
@if(WILDERNESS_EDIT_OVERLAY)
@group(constants::MATERIAL_BIND_GROUP) @binding(115) var edit_overlay_texture: texture_2d<f32>;

// Stamp preview heightfield (`editing`): composited in `fn vertex` while
// flags bit5 is set — the GPU floating preview that makes half-map stamps
// per-frame free. Shares the heightmap sampler like the overlay.
@if(WILDERNESS_STAMP)
@group(constants::MATERIAL_BIND_GROUP) @binding(116) var stamp_texture: texture_2d<f32>;

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

// Terrain height (world Y) at world `xz` from the heightmap: wrapped when
// looping, clamped to the coverage otherwise.
fn base_height_at(xz: vec2<f32>) -> f32 {
    let world_size = texel_size * vec2<f32>(textureDimensions(heightmap_texture));
    let uv = xz / world_size + 0.5;
    let looping = (flags & 8u) != 0u;
    let sample_uv = select(clamp(uv, vec2(0.0), vec2(1.0)), uv, looping);
    return height_bilinear(sample_uv, 0) * (minmax.y - minmax.x) + minmax.x;
}

// --- Geomorphing ---
// Each ring has half the resolution of the one inside it, so where a ring
// meets the next coarser one the surface changes detail abruptly — and since
// rings snap with the camera, that seam jumps as you move: LOD popping, worst
// in VR where the head never stops. Across the outer band of each ring every
// vertex blends toward the height the *coarser* ring renders at that spot, so
// at the seam the two match exactly and resolution changes continuously
// (Losasso & Hoppe, GPU Gems 2 clipmaps; CDLOD morphing). The band is placed
// by distance from the camera, assumed to be the clipmap target (a third-
// person target far from the camera would morph in the wrong place). In
// stereo each eye's position differs by ~3 cm, so the eyes' morphs differ by
// well under a millimetre; measuring from the head (the target) would make
// them identical — see the roadmap's VR rendering path.
const MORPH_BAND: f32 = 0.3; // fraction of the ring's half-width

// The coarser ring's surface at fine vertex `xz` (a multiple of `cell`), whose
// own height is `fine`: coarse vertices lie every 2 cells, so a fine vertex is
// either one of them (same height — no extra reads), the middle of a coarse
// edge (average of its ends), or a coarse quad's center — on the quad's
// (+x, +z) diagonal, the ring meshes' triangulation.
fn coarse_height_at(xz: vec2<f32>, cell: f32, fine: f32) -> f32 {
    let k = vec2<i32>(round(xz / cell));
    let odd = vec2<bool>((k.x & 1) != 0, (k.y & 1) != 0);
    if odd.x && odd.y {
        return 0.5 * (base_height_at(xz - vec2<f32>(cell)) + base_height_at(xz + vec2<f32>(cell)));
    }
    if odd.x {
        let dx = vec2<f32>(cell, 0.0);
        return 0.5 * (base_height_at(xz - dx) + base_height_at(xz + dx));
    }
    if odd.y {
        let dz = vec2<f32>(0.0, cell);
        return 0.5 * (base_height_at(xz - dz) + base_height_at(xz + dz));
    }
    return fine;
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
    var world_y = base_height_at(out.world_position.xz);

    // Geomorph toward the coarser ring across this ring's outer band.
    let level_cell = length(model[0].xyz);
    let ring = dev.ring_half_cells * level_cell;
    // The seam sits `ring` from the snapped ring center, which trails the
    // camera by up to a snap step (≤ 2 cells): finish morphing before it.
    let morph_end = ring - 4.0 * level_cell;
    let morph_start = morph_end - MORPH_BAND * ring;
    let to_camera = abs(out.world_position.xz - view.world_position.xz);
    let morph = saturate((max(to_camera.x, to_camera.y) - morph_start) / (morph_end - morph_start));
    if morph > 0.0 {
        world_y = mix(world_y, coarse_height_at(out.world_position.xz, level_cell, world_y), morph);
    }

    // Stamp floating preview (flags bit5): add the stamp heightfield
    // through its world transform, exactly what a commit will write into the
    // f32 field — same bilinear (texel centers at half-integers, clamped),
    // same mask weighting, same encode-range clamp. Keep this in lockstep
    // with the editor's CPU apply or the commit pops.
    @if(WILDERNESS_STAMP)
    if (flags & 32u) != 0u {
        var rel = out.world_position.xz - dev.stamp_center;
        if looping {
            // Shortest toroidal offset, so a stamp near the seam previews on
            // the far side (and on the visible repeats).
            rel -= world_size * round(rel / world_size);
        }
        let c = cos(dev.stamp_rotation);
        let s = sin(dev.stamp_rotation);
        // World → stamp-local (inverse of the stamp's +Y rotation).
        let local = vec2(c * rel.x + s * rel.y, -s * rel.x + c * rel.y);
        let stamp_uv = local / (dev.stamp_half_size * 2.0) + 0.5;
        if all(stamp_uv > vec2(0.0)) && all(stamp_uv < vec2(1.0)) {
            var weight = 1.0;
            // Mask-confined (bit6): weight by the mask overlay, sampled the
            // same wrapped way the fragment tints it.
            @if(WILDERNESS_EDIT_OVERLAY)
            if (flags & 64u) != 0u {
                let overlay_uv = select(clamp(height_uv, vec2(0.0), vec2(1.0)), fract(height_uv), looping);
                weight = textureSampleLevel(edit_overlay_texture, heightmap_sampler, overlay_uv, 0.0).r;
            }
            let stamp_h = textureSampleLevel(stamp_texture, heightmap_sampler, stamp_uv, 0.0).r;
            world_y = clamp(world_y + dev.stamp_strength * stamp_h * weight, minmax.x, minmax.y);
        }
    }

    // Out past the heightmap coverage the coarse LOD skirt would render as a wall
    // at the edge height. Drop those vertices to the height floor so the stripe
    // stays low and out of sight (the sample above is clamped, not read OOB).
    // When looping there is no "outside" — the tiled heightmap covers every ring.
    let in_coverage = all(height_uv >= vec2(0.0)) && all(height_uv <= vec2(1.0));
    out.world_position.y = select(minmax.x, world_y, in_coverage || looping);

    // Collapse every triangle touching a vertex inside a hole: a NaN clip
    // position is culled before rasterization, so no fragment work and no
    // early-Z loss. Tested on world XZ (wrapped to the canonical tile when
    // looping) with the ring's cell size read off the instance transform —
    // both view-independent, so stereo eyes cull identically.
    @if(WILDERNESS_HOLES) {
        let cell = length(model[0].xyz);
        var p = out.world_position.xz;
        if looping {
            p -= world_size * round(p / world_size);
        }
        for (var i = 0u; i < dev.hole_count; i++) {
            let a = dev.holes.shapes[i * 2u];
            let b = dev.holes.shapes[i * 2u + 1u];
            if b.w < HOLE_MIN_CELLS * cell {
                continue;
            }
            let d = p - a.xy;
            var inside = false;
            if b.z < 0.5 {
                let u = d.x * a.z + d.y * a.w;
                let v = -d.x * a.w + d.y * a.z;
                inside = abs(u) <= b.x && abs(v) <= b.y;
            } else {
                inside = dot(d, d) <= b.x * b.x;
            }
            if inside {
                out.position = vec4<f32>(bitcast<f32>(0x7fc00000u));
                return out;
            }
        }
    }

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
@if(WILDERNESS_SHADE) {

// Terrain PBR lighting with the ground's sun visibility injected on the directional sun.
// Compact fork of `apply_pbr_lighting`: base layer only, directional + clustered
// point/spot lights + ambient + environment map (no clearcoat / transmission /
// anisotropy — the terrain doesn't use them). `sun_vis` (0..1) attenuates only the
// direct sun, so shadowed valleys still receive sky/ambient light; point and spot
// lights are unaffected by it (it's the sun's occlusion, not theirs).
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

// `baked`, when `found`, replaces the environment map's diffuse and the flat
// ambient; the caller has already taken its sun visibility into `sun_vis`.
fn terrain_apply_lighting(in: pbr_types::PbrInput, sun_vis: f32, baked: BakedSample) -> vec4<f32> {
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
            shadow *= in.directional_shadow_factor;
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
        direct += lighting::point_light(light_id, &li, baked_diffuse(light_id, baked), true) * shadow;
    }

    // Spot lights, likewise shadowed per-light.
    for (var i = ranges.first_spot_light_index_offset; i < ranges.first_rect_light_index_offset; i++) {
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
        direct += lighting::spot_light(light_id, &li, baked_diffuse(light_id, baked)) * shadow;
    }

    // Indirect: baked light, else the environment map (the physical-sky IBL)
    // + flat ambient. Colored multi-bounce diffuse occlusion + AO-derived
    // specular occlusion from the detail AO, which is finer than either.
    var indirect = vec3<f32>(0.0);
    let ao_scalar = in.diffuse_occlusion.r;
    let diffuse_occ = gtao_multi_bounce(ao_scalar, diffuse_color);
    var spec_occ = specular_occlusion_from_ao(NdotV, ao_scalar, roughness);
    if baked.found {
        indirect += diffuse_color * baked.light * diffuse_occ;
        // Reflections of the environment map are of the open sky: keep only
        // the share of it the baked light says reaches here.
        if dev.baked.sky_luminance > 0.0 {
            let luminance = dot(baked.light, vec3<f32>(0.2126, 0.7152, 0.0722));
            spec_occ *= saturate(luminance / dev.baked.sky_luminance);
        }
    }
    @if(ENVIRONMENT_MAP) {
    let env = environment_map::environment_map_light(&li, &ranges, false);
    if !baked.found {
        indirect += env.diffuse * diffuse_occ;
    }
    indirect += env.specular * spec_occ;
    }
    if !baked.found {
        // Flat ambient ignores the normal entirely; keep it on the shading normal.
        indirect += ambient::ambient_light(in.world_position, in.N, in.V, NdotV, diffuse_color, F0, perceptual_roughness, in.diffuse_occlusion);
    }

    let emissive = in.material.emissive.rgb * base_color.a;
    var color = view.exposure * (direct + indirect) + emissive;
    if fog.density + fog.haze_density > 0.0 {
        color = apply_height_fog(fog, color, view.world_position, in.world_position.xyz, view.exposure);
    }
    return vec4<f32>(color, base_color.a);
}

// Whether clustered light `light_id` lights the ground's diffuse: not where
// baked light holds it already, as Bevy leaves a lightmapped mesh's diffuse
// to its lightmap for a light without `affects_lightmapped_mesh_diffuse`.
fn baked_diffuse(light_id: u32, baked: BakedSample) -> bool {
    return !baked.found
        || (clustered_lights.data[light_id].flags & mesh_view_types::POINT_LIGHT_FLAGS_AFFECTS_LIGHTMAPPED_MESH_DIFFUSE_BIT) != 0u;
}

// The baked light at a point: irradiance / π re-aimed at the shading normal,
// and the sun visibility.
struct BakedSample {
    light: vec3<f32>,
    sun_visibility: f32,
    found: bool,
}

fn no_baked_light() -> BakedSample {
    return BakedSample(vec3<f32>(0.0), 1.0, false);
}

// Region `i`'s colour and direction at world `xz`, clamped half a texel inside
// its tiles so filtering never reads a neighbour's.
fn baked_region_sample(i: u32, xz: vec2<f32>, n: vec3<f32>) -> vec4<f32> {
    let region = dev.baked.regions[i];
    let local = (xz - region.world.xy) / (region.world.zw - region.world.xy);
    let half_texel = 0.5 / vec2<f32>(textureDimensions(baked_light_atlas));
    let size = region.color.zw - region.color.xy;
    let inner = clamp(local * size, half_texel, size - half_texel);
    let color = textureSampleLevel(baked_light_atlas, rvt_albedo_sampler, region.color.xy + inner, 0.0);
    let direction = textureSampleLevel(baked_light_atlas, rvt_albedo_sampler, region.direction.xy + inner, 0.0);
    // Mirrors `level_material.wesl`: `(0.5·n·d + 0.5) / w` is 1 at the
    // baked normal and follows the shading normal elsewhere.
    let d = normalize(direction.xyz * 2.0 - 1.0);
    let scale = (0.5 * dot(n, d) + 0.5) / max(direction.w, 0.05);
    return vec4<f32>(color.rgb * scale, color.a);
}

fn baked_region_contains(i: u32, xz: vec2<f32>) -> bool {
    let world = dev.baked.regions[i].world;
    return all(xz >= world.xy) && all(xz <= world.zw);
}

// The last region covering `xz` wins, faded in from its edge over
// `patch_fade` against whatever lies beneath.
fn baked_light_at(xz: vec2<f32>, n: vec3<f32>) -> BakedSample {
    let count = dev.baked.count;
    if count == 0u || !baked_region_contains(0u, xz) {
        return no_baked_light();
    }
    var result = baked_region_sample(0u, xz, n);
    for (var i = 1u; i < count; i++) {
        if !baked_region_contains(i, xz) {
            continue;
        }
        let world = dev.baked.regions[i].world;
        let edge = min(min(xz.x - world.x, world.z - xz.x), min(xz.y - world.y, world.w - xz.y));
        let weight = smoothstep(0.0, dev.baked.patch_fade, edge);
        result = mix(result, baked_region_sample(i, xz, n), weight);
    }
    return BakedSample(result.rgb * dev.baked.exposure, result.a, true);
}

// The ground's sun shadow, from the field that shadows meshes, read a little
// above the surface: a lit column's ceiling is its own ground, so at the surface
// the field reads half shadow, and on a slope the bilinear ceiling sits higher
// still, by up to a field texel times the slope. `uv` is the RVT's, which the
// field shares (both cover the terrain), wrapping with the same sampler.
fn ground_sun_visibility(world: vec3<f32>, normal: vec3<f32>, uv: vec2<f32>) -> f32 {
    if dev.sun_shadow.valid < 0.5 {
        return 1.0;
    }
    let looping = (flags & 8u) != 0u;
    if !looping && (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
        return 1.0;
    }
    let field_texel = 1.0 / (dev.sun_shadow.inv_world_size * f32(textureDimensions(sun_shadow_field).x));
    let slope = sqrt(max(1.0 - normal.y * normal.y, 0.0)) / max(normal.y, 0.25);
    let lift = 2.0 * MIN_PENUMBRA + field_texel * slope;
    return sun_visibility_at(sun_shadow_field, rvt_albedo_sampler, dev.sun_shadow, uv, world.y + lift);
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

    // Clay display mode (flags bit4): shading is stale — unbaked edits,
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
        @if(WILDERNESS_EDIT_OVERLAY) {
        let clay_overlay_uv = select(clamp(uv, vec2(0.0), vec2(1.0)), fract(uv), (flags & 8u) != 0u);
        let clay_overlay = textureSample(edit_overlay_texture, heightmap_sampler, clay_overlay_uv).r;
        albedo = mix(albedo, vec3<f32>(1.0, 0.25, 0.05), clay_overlay * 0.5);
        }
        var clay_pbr = pbr_input_from_standard_material(clay_in, is_front);
        clay_pbr.material.base_color = vec4<f32>(albedo, 1.0);
        clay_pbr.material.perceptual_roughness = 0.95;
        clay_pbr.material.metallic = 0.0;
        clay_pbr.diffuse_occlusion = vec3<f32>(1.0);
        @if(PREPASS_PIPELINE)
        let out = deferred_output(clay_in, clay_pbr);
        @if(!PREPASS_PIPELINE)
        var out: FragmentOutput;
        @if(!PREPASS_PIPELINE) {
        // Sun visibility 1.0: there's no valid bake to shadow with.
        out.color = terrain_apply_lighting(clay_pbr, 1.0, no_baked_light());
        out.color = main_pass_post_lighting_processing(clay_pbr, out.color);
        }
        return out;
    }

    // Sample the baked RVT instead of blending the splat per-fragment:
    // albedo + cavity (alpha); octahedral world normal + roughness + packed
    // material ids (alpha, read NEAREST below).
    let rvt_a = textureSample(rvt_albedo_texture, rvt_albedo_sampler, uv);
    let rvt_n = textureSample(rvt_normal_texture, rvt_albedo_sampler, uv);
    let base_normal = oct_decode(rvt_n.rg);
    // Baked light needs the shading normal, so it's sampled after the detail
    // overlay below; until then the shadow field's sun visibility stands.
    var sun_vis = ground_sun_visibility(in.world_position.xyz, base_normal, uv);

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
        // The RVT already holds this same albedo array blended at macro tiling, so
        // the detail sample must contribute *variation*, not colour, or the two
        // multiply into albedo squared. Its coarsest mip is the layer's own mean,
        // so the grain below is unit-mean by construction and leaves the macro
        // albedo intact for any texture set, whatever its average brightness.
        let da_lod = f32(textureNumLevels(detail_albedo_array) - 1u);
        var dn = textureSampleGrad(detail_normal_array, detail_albedo_sampler, dtile, id0, ddx, ddy).xyz * 2.0 - 1.0;
        var da = textureSampleGrad(detail_albedo_array, detail_albedo_sampler, dtile, id0, ddx, ddy).rgb;
        var da_avg = textureSampleLevel(detail_albedo_array, detail_albedo_sampler, dtile, id0, da_lod).rgb;
        var dorm = textureSampleGrad(detail_orm_array, detail_albedo_sampler, dtile, id0, ddx, ddy);
        if (flags & 4u) == 0u && mblend > 0.0 {
            let dn1 = textureSampleGrad(detail_normal_array, detail_albedo_sampler, dtile, id1, ddx, ddy).xyz * 2.0 - 1.0;
            let da1 = textureSampleGrad(detail_albedo_array, detail_albedo_sampler, dtile, id1, ddx, ddy).rgb;
            let da1_avg = textureSampleLevel(detail_albedo_array, detail_albedo_sampler, dtile, id1, da_lod).rgb;
            let dorm1 = textureSampleGrad(detail_orm_array, detail_albedo_sampler, dtile, id1, ddx, ddy);
            dn = mix(dn, dn1, mblend);
            da = mix(da, da1, mblend);
            da_avg = mix(da_avg, da1_avg, mblend);
            dorm = mix(dorm, dorm1, mblend);
        }
        // Flip X (see bake) and reorient onto the base normal.
        dn = dn * vec3<f32>(-1.0, 1.0, 1.0);
        let dn_scaled = vec3<f32>(dn.xy * detail.normal_strength * detail_fade, dn.z);
        let ref_axis = select(vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(1.0, 0.0, 0.0), abs(base_normal.z) > 0.99);
        let dt = normalize(cross(ref_axis, base_normal));
        let db = cross(base_normal, dt);
        world_normal = normalize(dt * dn_scaled.x + db * dn_scaled.y + base_normal * dn_scaled.z);

        // Grain is the detail's *brightness* around its own mean, capped. A
        // per-channel ratio amplified grass's near-zero blue up to ~6×, and
        // where the NEAREST material id and the filtered colour disagree (a
        // grass id over rock colour) that tinted rock purple. Brightness can't
        // change hue, whatever colour it lands on.
        let luma = vec3<f32>(0.2126, 0.7152, 0.0722);
        let grain = min(dot(da, luma) / max(dot(da_avg, luma), 1e-4), 3.0);
        albedo *= mix(1.0, grain, detail.albedo_strength * detail_fade);
        rough = mix(rvt_n.b, dorm.g, detail_fade);
        ao = mix(1.0, dorm.r, detail_fade);
    }
    in_modified.world_normal = world_normal;

    @if(WILDERNESS_EDIT_OVERLAY) {
    // Editor mask visualization: tint the albedo where the overlay is painted.
    // Applied to albedo (not post-lighting) so it shades plausibly and works in
    // the deferred path too. `fract` wraps the sample on looping terrain.
    let overlay_uv = select(clamp(uv, vec2(0.0), vec2(1.0)), fract(uv), (flags & 8u) != 0u);
    let overlay = textureSample(edit_overlay_texture, heightmap_sampler, overlay_uv).r;
    albedo = mix(albedo, vec3<f32>(1.0, 0.25, 0.05), overlay * 0.5);
    }

    var pbr_input = pbr_input_from_standard_material(in_modified, is_front);
    pbr_input.material.base_color = vec4<f32>(albedo, 1.0);
    pbr_input.material.perceptual_roughness = rough;
    pbr_input.material.metallic = 0.0;
    // The detail AO darkens only ambient/indirect, never the direct sun.
    pbr_input.diffuse_occlusion = vec3<f32>(ao);
    let baked = baked_light_at(in.world_position.xz, pbr_input.N);
    if baked.found {
        sun_vis = baked.sun_visibility;
    }

    @if(PREPASS_PIPELINE)
    let out = deferred_output(in_modified, pbr_input);
    @if(!PREPASS_PIPELINE)
    var out: FragmentOutput;
    // Debug: output one value unlit so the tonemapper shows it as a literal
    // grayscale value.
    @if(!PREPASS_PIPELINE)
    if dev.debug_view == 1u {
        out.color = vec4<f32>(vec3<f32>(rvt_a.a), 1.0);
    } else if dev.debug_view == 2u {
        out.color = vec4<f32>(vec3<f32>(sun_vis), 1.0);
    } else {
        out.color = terrain_apply_lighting(pbr_input, sun_vis, baked);
        out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    }

    return out;
}

}  // @if(WILDERNESS_SHADE)
