# Terrain Material & Shadowing — Design Doc

Status: **Draft / agreed direction** · Last updated: 2026-07-12

This document records the decisions for adding an Unreal-style multi-material
landscape system (splat-blended textures) and a terrain shadowing pipeline to
`bevy-clipmap`. It captures *what* we're building, *why*, and the *order* to
build it. It is the reference for future sessions and contributors.

> **2026-07-08 update.** A camera-centered **toroidal RVT ring** (double-buffered,
> amortized strip bake) and a decoupled **shadow + sky-AO** texture were built,
> evaluated, and **shelved** — see §2.1 and §4.2 for the verdicts. The adopted
> baseline stays the **static bake-once RVT**, now at **8192²** (~1 m/texel).
> Since then: material placement went **fully procedural** (slope/height bands,
> control map removed, §3.2), the **macro albedo map was removed** (§3.2), the
> detail overlay **blends the top-2 materials** (§3.5), and **no realtime
> shadows** became a governing rule (§4.2). Landscapes are **user-generated**,
> which drives the procedural/no-authored-maps direction throughout.

---

## 1. Goals

- **Unreal-style landscape multi-material**: blend multiple textures (grass,
  rock, dirt, snow, …) across the terrain — placed **procedurally** from slope
  and height (no authored control map; landscapes are **user-generated**, so
  nothing may require hand-painted per-terrain data).
- **Two view distances, both beautiful**:
  - **Close range** convincing enough for **VR** (no visible tiling, high texel
    density, stable — no shimmer).
  - **Long range** for beautiful vistas.
- **VR is the primary target.** Flatscreen players get an **optional higher
  visual tier**.
- **Static objects cast shadows onto the terrain** (buildings, rocks, props).
- Stay a reusable, published crate — features should be opt-in.

### Non-goals (for now)
- Dynamic time-of-day sun (deferred — see §5, phase "Later").
- Streamed/virtual *source* data for planet-scale worlds (deferred — see §8).
- **Realtime shadows/AO of any kind** — CSM, SSAO/GTAO, live ray-march, VSM,
  ray-traced. Governing rule, not just a deferral: everything static bakes once;
  anything dynamic gets **faked** (blob/decal shadows). See §4.2.

---

## 2. Chosen architecture — Runtime Virtual Texture (RVT), "Tier 2 / AAA"

We use the **RVT approach**: rather than blending N material layers every frame
for every pixel, **bake the blended result into a texture** and have the main
terrain pass do a single fetch. This **decouples per-pixel shading cost from
material complexity** — the lever that lets one material serve both a tight VR
budget and a lavish flatscreen budget (see §7).

**The world is finite-ish, so the RVT is a static, full-terrain, bake-once
texture — NOT a camera-centered clipmap.** (Re-affirmed after actually building
the toroidal alternative — see §2.1.) The targets are **8192²** over the ~8192 m
example world → **~1 m/texel** (bumped from 4096²/2 m; sampling cost is
unchanged, VRAM ~4×, judged worth it for the mid-ground). What gets baked:

- Albedo (sRGB) + **sun-visibility** in alpha (baked shadow — see §4)
- Octahedral world normal + roughness + **packed material ids** in alpha (§3.5)

The bake runs via top-down orthographic cameras → `RenderTarget::Image`, then
stops. The main `terrain.wgsl` pass collapses from ~14 samples to ~2 fetches
(+ the near-only detail samples, §3.5) — the unused source-array bindings were
removed from the main material, freeing 4 sampled-texture slots against the
~16-per-stage ceiling on mobile-VR GPUs.

**Bake lifecycle — sentinel-gated, not frame-counted.** A camera that renders
before its pipeline is compiled and source textures are GPU-resident bakes an
empty (black) RVT → the terrain reads as a chrome mirror (`metallic=1`,
`roughness=0`). A fixed frame-count wait is a machine-dependent guess (and a slow
one — the shadow-march bake is expensive per frame). Instead each bake camera
starts **inactive behind a sentinel**: the same bake material drawn to a 4×4
readback target. The first **non-black readback** proves the pipeline is compiled
and textures are up, so the full-res bake fires for ~2 frames, then everything
deactivates and the sentinel entities despawn. (`init_rvt` / `drive_rvt_bake`.)

**Close-up detail does NOT come from RVT density.** The RVT caches at a fixed
texel density, so sub-cm rock/sand detail is impossible at any affordable
resolution. Close-range fidelity is a *separate* **near-range detail overlay**
(§3.5); RVT density only controls the base material and material-boundary
sharpness.

### 2.1 Camera-centered ring RVT — BUILT, EVALUATED, SHELVED (2026-07-08)

A camera-following high-density ring (1024 m footprint, 0.25 m/texel) was fully
prototyped on an exploration branch: ring-relative UV + ring→macro fade,
target-following re-bake, **double-buffering** (bake into a back buffer, swap on
completion — no tear), and an **amortized bake** (interleaved scanlines over N
frames into a non-cleared target — no hitch). It worked and looked good.

**Shelved for the VR baseline anyway.** The ring re-bakes whenever the head
moves — recurring per-frame GPU work that competes with the scene render inside
a Quest-class 8–11 ms budget, unverifiable without on-device profiling — and
double-buffering doubles ring VRAM. The static bake-once RVT has **zero**
per-frame bake cost, the safest possible VR profile; at 8192² its mid-ground is
good enough that the ring's extra density wasn't missed. Classic
sharpness-vs-schedulability trade — VR chose schedulability.

Revisit only if: the world outgrows a static RVT (>10–15 km at 1 m/texel),
terrain becomes streamed/effectively infinite, or a **flatscreen tier** wants the
extra near density (§7). The prototype (including the multi-ring "texture
clipmap riding the geometry levels" design) lives in this doc's git history and
the abandoned exploration commits.

---

## 3. Material system — procedural splat blending

### 3.1 Layer configuration

Layers are configured on the `Clipmap` component (`TerrainLayer`): per-layer
`tiling_scale`, `height_blend`, `normal_strength`, `roughness`, plus the
placement bands below. (The earlier `TerrainLayerSet` hot-reloadable-asset idea
and RGBA `control_maps` are superseded — see §3.2.)

### 3.2 Placement & blending rules — procedural, no control map (2026-07-08)

**The painted RGBA control map is REMOVED.** Landscapes are user-generated, so
placement must derive from the terrain itself. Each layer's weight is computed
in the bake as the overlap of two optional **bands** (`band()` in `bake.wgsl`):

- **`SlopeRule { min_deg, max_deg, blend_deg }`** — slope-angle band: grass on
  flat, dirt mid-slope, rock steep. Weight is 1 inside the band, ramping out
  over `blend_deg` at each edge.
- **`HeightRule { min, max, blend }`** — world-height band: snow above a
  snowline, cliffs staying bare rock at any height.
- A layer with neither rule is present **everywhere** — the "base layer"
  pattern: giving grass no slope band lets it compete on cliffs and poke through
  the rock via height-blend scoring, which is what makes boundaries look natural
  (an exclusive-bands-only setup read as sterile).
- **Height/depth blending** (not linear alpha) for crisp transitions, as before.
- **Gotcha — do NOT jitter the band inputs with value noise.** An fbm-of-value-
  noise jitter was added to make boundaries wander; thresholding lattice noise at
  a boundary makes the edge hug the noise's square cells → meters-scale square
  "brush-stroke" patchwork at **every** material edge. Removed; the base-layer
  competition provides the organic look on its own. If boundary wander is ever
  wanted, use a lattice-free (gradient, rotated-octave) noise.
- **Macro variation map — REMOVED.** With real CC0 texture sets + hex de-tiling,
  the layers carry the look; the macro multiply and the distance→macro fade were
  dead weight (and a texture slot). `Clipmap::{color, macro_*}` deleted.

**Guardrails (baked in):** arrays are `Handle<Image>`, so BC7/BC5 mipmapped KTX2
work unchanged; the tiling sampler uses repeat + anisotropic filtering; albedo is
sampled sRGB, height linear; weights are normalized in-shader; extending past 4
layers means widening the `TerrainParams` band vectors (top-4 per pixel).
**Normal maps are OpenGL convention (`nor_gl`)**, and the bake/overlay flip the
normal's X to match the reorientation tangent (which runs −X vs the +X tiling
UV) — without the flip, cracks light as ridges. Documented in the README.

### 3.3 Close-range VR fidelity

The RVT gives a cheap base everywhere; close-up crispness comes from the detail
overlay (§3.5). The remaining near-range items:

- **Stochastic / hex tiling** to eliminate visible repetition in the *baked base*.
  ~3× samples + `textureSampleGrad`, so it runs **inside the RVT bake** (§2,
  `bake.wgsl`), not per-fragment — amortized to ~zero per-frame. ✅ done.
  - **Gotcha — hex cell size (`triangle_grid`).** Cells must span **~1 texture
    repeat**. Sub-repeat cells randomize the UV every few RVT texels, so under
    the bake's heavy minification the base collapses into per-texel speckle
    ("looks like noise"); multi-repeat cells let the in-cell repetition show.
    ~1 repeat = every cell is a distinct crop, no visible tiling, structure intact.
- ~~**Distance→macro vista blend**~~ — built, then **removed with the macro map**
  (§3.2): hex de-tiling + the 8K RVT carry the vistas without it.
- **Triplanar on steep slopes** — **deferred as a stretch goal** (performance
  first). It does *not* amortize into the RVT bake (RVT is XZ-parameterized, so
  cliffs are its inherent weak spot) and would stay a live per-frame cost. If
  revisited, slope-gated biplanar (flat stays 1× planar; only cliffs pay) on
  albedo + normal. Heightfields can't do overhangs anyway, so cliff stretch is a
  bounded artifact.

### 3.4 Precomputed normals ✅

The RVT bakes the reoriented world normal (octahedral), so the main pass samples
it instead of reconstructing from heightmap derivatives per-fragment. Removes the
wasted samples **and the shimmer source VR exaggerates**.

### 3.5 Near-range detail overlay — close-up VR fidelity

The RVT base can't represent sub-cm surface detail (fixed texel density), so
"real rocks and sand up close" comes from **high-frequency detail textures blended
onto the RVT sample, near the camera only, faded with distance** — the standard
AAA detail-mapping / macro-micro technique. Cheap: a couple of extra samples that
only matter on near pixels.

- **v1 — generic detail** ✅ done: a high-frequency detail *normal* + detail
  albedo grain tiled small (~1.5 m), blended onto the RVT normal / albedo in the
  main pass, faded out by camera distance (`detail_near`/`detail_far`). The
  detail normal is reoriented onto the RVT world normal.
- **v2 — per-material detail** ✅ done (the full "real rocks/sand"): per-material
  detail albedo + normal + ORM arrays, selected by a layer ID baked into the
  RVT's metallic slot (terrain is never metallic). Real photographic detail
  textures drop straight into the same `Handle<Image>` array slots.
- **v3 — top-2 material blend + far-field skip** ✅ done (2026-07-08). A single
  dominant ID snapped at material boundaries (dirt wearing rock's relief; hard
  detail seams while the base blended smoothly underneath). Now the bake packs
  the **two dominant layer indices + a 4-bit blend weight into the material-id
  byte (2+2+4)** and the main pass lerps both materials' detail normal / albedo
  / ORM. Two hard-won gotchas:
  - **Read the packed byte NEAREST (`textureLoad`).** Bilinear-filtering a
    packed id byte sweeps through garbage id/weight combos → banding strips.
    Per-texel selection steps at ~1 m instead, which reads as natural mottling.
  - **The whole detail block is gated behind `detail_fade > 0`** with derivatives
    hoisted out of the branch (`textureSampleGrad`) for correct mips — far
    terrain now pays **zero** detail samples. Net: near +2 samples for the
    blend, far −3.

### 3.6 Loading real texture sets ✅

`load_terrain_array(images, paths, srgb)` (public API) decodes one image file per
layer and stacks them into the tiling `2d_array` the layer/detail slots expect.

- **Generates a full mip chain on load.** Runtime-decoded PNGs carry no mips; the
  RVT bake samples the arrays heavily minified, so without mips real textures
  alias into per-texel noise. sRGB layers are averaged in ~linear space.
- All layer files must share dimensions (decode-only, no resample); `srgb` picks
  color vs linear (normal / ORM) space; **ORM = occlusion, roughness, metallic in
  R, G, B** (metallic ~0 for terrain).
- `assets/fetch_textures.py` pulls CC0 sets from Poly Haven, converts to 8-bit,
  and packs ORM. Textures stay out of git (`assets/terrain/` is `.gitignore`d).
- Dev-profile note: `png`/`image`/`fdeflate`/`miniz_oxide` get `opt-level = 3` in
  `Cargo.toml` — debug-mode PNG inflate made example startup ~30 s.

---

## 4. Lighting & shadows — baked baseline (fixed sun)

Everything here is **baked** — see the governing rule in §4.2.

Two distinct shadow problems, two (baked) tools:
- **Terrain self-shadow** (mountains → valleys): **heightfield ray-march** — march
  the heightmap toward the sun and test occlusion, once, in the bake. A shadow map
  is the *wrong* tool here (resolution / peter-panning, and it would need the
  displaced terrain to cast).
- **Objects → terrain** (buildings, rocks, props): render the static props from
  the sun **during the bake** into the same sun-visibility channel (step 7). Not
  CSM — realtime shadow maps are ruled out (§4.2).

**Baked baseline (cheapest, VR-ideal).** Because the RVT is static + finite, run
the heightfield ray-march **once, in the RVT bake**, against a **fixed sun**, and
store it in a **sun-visibility channel**. Terrain self-shadow then costs one
texture read per frame — zero per-frame shadow work, perfectly stable (no shimmer).
Static objects bake into the same channel (render them from the sun during the
bake).

**Terrain self-shadow ✅ done** (`bake.wgsl` `sun_visibility`, stored in the RVT
albedo target's alpha). The bake's sun direction is **detected from the scene's
`DirectionalLight`** (its `back()` = direction *toward* the sun), read once in
`init_rvt` — not a config field, so the bake can never disagree with the light
that actually renders the scene. Implementation notes:
- The march starts biased along the surface normal and uses **adaptive
  (geometric) step size** — fine near-field so grazing sun-facing slopes don't
  self-shadow (acne), growing steps for reach (`MAX_DIST` 6000 covers low-sun
  long casts). Soft penumbra via a clearance/distance ramp.
- **Applied through a compact lighting fork** (`terrain.wgsl`
  `terrain_apply_lighting`): base layer + directional + clustered point/spot lights
  + ambient + env map, with `sun_vis` multiplied onto **only the direct sun term**
  so shadowed areas keep sky/ambient (not black). Point/spot lights are *not*
  attenuated by `sun_vis` — it's the sun's baked occlusion, not theirs. No stock
  hook injects a per-pixel shadow factor, hence the small fork (§6.1). Much smaller
  than the old horizon fork.
- **Orientation gotcha:** the bake camera's `up` is `-Z` so the RVT texel layout
  matches the main pass's `world_xz / world_size + 0.5` sampling. A `+Z` up
  stored every RVT channel (shadow, normal, material ID) rotated 180°.

### 4.1 Horizon map — REMOVED

The FFT horizon map is **removed**. Decision rationale:

- It exists to answer "is the sun occluded by distant terrain, for an
  **arbitrary** sun direction, cheaply." With a **fixed sun**, that per-direction
  data is redundant — a baked shadow texture that includes the terrain contains
  the exact same self-shadowing, for one direction, at a fraction of the storage
  (`W·H` vs the horizon map's `360·W·H·4`) and per-fragment cost (one fetch vs
  the `reconstruct_horizon` sample loop).
- **This is a permanent removal, not delete-now-re-add-later.** The future
  dynamic-sun path (§5) uses **on-the-fly ray-marched heightfield shadows**,
  which re-derive self-shadowing live — an *alternative* to the horizon map, not
  a consumer of it. The horizon map is part of neither phase.

**Removal touches:**
- `src/lib.rs`: `Clipmap.horizon` / `horizon_coeffs` fields, `GridMaterial`
  bindings 104/105/106, and the two material-construction sites passing them.
- `src/terrain.wgsl`: bindings 104–106, `reconstruct_horizon` (130-142), and the
  horizon block at 416-428 — `min(shadow, horizon_shadow)` simplifies to
  `shadow` (later `min(shadow, baked_static_shadow)`).
- `convert/clipmap.py`: the `horizon` subcommand.
- `examples/basic.rs` + `README.md`: horizon asset load and docs.

### 4.2 Governing rule — NO realtime shadows/AO; and the sky-AO verdict

**Hard constraint (2026-07-08): avoid realtime shadows and occlusion at all
costs.** VR is ~90–120 Hz × two eyes; a per-frame shadow pass risks hitches, and
shadow shimmer is nausea-inducing in a headset. This rules out *every* per-frame
technique — CSM, SSAO/GTAO, live heightfield march, VSM, ray-traced. The system
is **one baked pass plus fakes**:

| Case                                | Approach (zero per-frame shadow work)              |
| ----------------------------------- | -------------------------------------------------- |
| Terrain self sun-shadow             | Baked sun-visibility (RVT alpha). ✅ done           |
| Static object → terrain sun-shadow  | Baked into the same channel (sun-view pass, step 7)|
| Static object's own shadow/AO       | Baked per-object (vertex AO / lightmap) at load    |
| **Dynamic** objects *casting* shadow | **Faked** — soft blob/decal. Never a shadow map.   |
| **Dynamic/flying** objects *receiving* terrain shadow | Per-entity sun-vis point query (§4.3) — O(entities), not a per-pixel pass |

**Sky-AO — tried and dropped (2026-07-08).** A horizon-march sky-occlusion bake
(`G` channel of a decoupled `RG8` shadow texture, applied to the ambient term
only) was built and evaluated. On open heightfield terrain it was **nearly
invisible** — hemispherical occlusion only bites in crevices/ravines, and open
ground has no nearby occluders — so it wasn't worth the extra channel and the
expensive multi-azimuth bake march. **Revisit when static objects exist**:
object-contact AO (the dark hug where a rock meets the ground, baked via
voxel/SDF or a hemispherical pass into that same channel) is very visible, and
that's when the channel earns its keep. The implementation is in the abandoned
exploration commits.

### 4.3 Dynamic & flying receivers — per-entity sun-visibility query (PRIORITY next)

> **Superseded as the mechanism by §4.5.** The per-entity heightfield march below
> is replaced by a single lookup of the terrain **shadow-height field** (§4.5):
> `terrain_vis(p) = smoothstep(H(p.xz) - penumbra, H(p.xz), p.y)`, one tap + a
> height compare, height-correct for every off-surface receiver (flyers *and* the
> static-item fragments of §4.5) with no per-entity march. The motivation below
> still holds — it's *why* a 3D query is needed; the field is *how*.

The baked sun-visibility channel is a **2D function of world XZ** — "is the
*terrain surface* at this point lit." It is correct only *on the surface it was
baked for*: a grounded character samples it (at their feet's XZ) and it just
works. **Flying characters break it** — at altitude a flyer can be lit while the
ground beneath sits in a mountain's shadow (or fly *into* a shadow volume high
up), and a ground-projected texture sampled at the flyer's XZ is wrong at every
non-zero height. Altitude demands a *3D* occlusion query, not a surface lookup.

**Tool — per-entity heightfield ray-march toward the fixed sun.** The sun is
fixed and the heightmap is resident, so from any 3D point you march the heightmap
toward the sun and test occlusion — the exact `bake.wgsl sun_visibility` logic,
seeded at the entity's *actual* world position instead of the surface. Height-
correct at any altitude, with no shadow-map resolution ceiling.

- **Not a shadow pass.** Cost scales with **entity count**, not screen pixels ×
  eyes × Hz — ~32–64 heightmap taps *per entity*. 100 characters ≈ ~5k texture
  reads/frame, rounding error in a VR frame. It is per-frame but **O(entities)**,
  so it does **not** fall under the §4.2 no-realtime-shadows rule (that bans
  per-*pixel* passes: CSM, SSAO, live full-screen march).
- **Cheaper still:** query only entities that moved (fixed sun + static terrain →
  a hovering entity's value is constant); amortize across N frames; compute
  **once per entity** and hand a scalar to the material — never put the march in
  the fragment shader, which reintroduces the per-pixel cost being avoided.
- **Continuous, not boolean.** Return the bake's soft 0..1 (its clearance/distance
  penumbra ramp), so a character crossing a shadow edge darkens over a soft band —
  no spatial flash. If throttled, **lerp / EMA** toward the new value so there's
  no temporal pop (one `f32` of state per entity). A single point-sample gives the
  whole character one uniform value; a 2-point feet+head sample is an optional
  refinement for a vertical gradient.

**Plugin/game boundary.** The **march is the plugin's** — it owns the data
(heightmap, the fixed sun it baked against, world extents) and exposes a reusable
`sample_sun_visibility(world_pos) -> f32` (CPU) plus a WGSL `#import` for GPU
materials. The **per-entity loop and material application are the game's** — the
plugin has no notion of "flyer" or "character." Same single-source-of-truth rule
as the sun-direction detection (§4): if the game reimplements the march it copies
the sun direction + heightmap + extents and drifts from what was actually baked.

**Buildings are a separate occluder.** The heightfield march only knows terrain —
a heightfield can't represent walls, overhangs, or interiors. Static buildings use
the **static sun depth map** (§4.2 step 7, baked as a sun-view *depth* map so it's
height-correct for flyers too), combined per query as `min(terrain_vis,
building_vis)`. The plugin owns the terrain half; the game bakes the building half
through plugin-provided hooks (the fixed-sun transform + the bake-timing signal).

### 4.4 Static-object shadows — buildings & trees (SEPARATE CRATE)

Static objects have a **bidirectional** relationship with terrain shadow: they
**cast** onto the terrain (and each other) and **receive** from mountains. Both
are static (fixed sun + static geometry), so both bake once and cost only a sample
at runtime — no per-frame shadow pass, the VR-critical property.

**Unified model — one scalar, two contributors.** Don't build "building shadows"
and "mountain shadows" as separate systems. Everything reduces to one value every
surface samples:

    sun_vis(world_pos) = min(terrain_vis, static_object_vis)

- Objects → terrain (cast): the terrain fragment samples `static_object_vis`.
- Terrain → objects (receive): the object fragment samples `terrain_vis` (§4/§4.3).
- Objects → objects, and either → flyers: the same `min()`, every surface samples
  both. That uniformity is the AAA-clean part — one lighting contract, not a matrix
  of special cases.

**Mechanism (VR-efficient).** `static_object_vis` is a **static sun-view depth
map** — render all static occluders depth-from-sun **once** at load into a cascaded
depth texture; per frame each receiver transforms into sun-clip space and does a
depth compare. Height-correct (works for flyers and vertical faces), zero per-frame
render. The entire buildings+trees system adds **~1 texture sample per fragment** at
runtime — nothing else.

- **Trees: bake the SDF, don't trace it.** Thin foliage aliases in a depth map;
  bake an SDF (UE Distance Field Shadows style) traced toward the sun at bake time,
  or cast from cheap capsule/billboard proxies. Either way runtime is a sample — a
  per-frame SDF trace is exactly the per-pixel cost the Quest can't afford.
- **AAA lineage, VR-tuned.** This is UE's baked-static shadow tier (Lightmass
  shadowmask), sampled live instead of burned into a lightmap. The VR constraint
  drops UE's dynamic tiers (VSM/CSM — the per-frame killers); genuinely dynamic
  casters (a flyer's shadow on the ground) stay blob/decal fakes (§4.2).

**Ownership — a separate crate, not this one, not in-game.** Static-object shadowing
operates on **arbitrary static meshes, not terrain**, so it stays out of
`bevy-clipmap` (same identity rule as everywhere). But it's a **reusable,
game-agnostic capability** ("bake static meshes' sun occlusion into a shared mask,
combined with terrain"), so it's a crate, not game code:

| Layer | Owns | Exposes / consumes |
| ----- | ---- | ------------------ |
| `bevy-clipmap` (this) | terrain + terrain sun-vis + the per-entity march | exposes: fixed sun frame, world bounds, heightmap, `ClipmapReady`, shadow-atlas handle |
| `bevy-static-sun-shadows` (new) | bakes arbitrary static meshes → sun depth map / SDF, sampled as a scalar | game-agnostic; consumes the shared sun frame |
| your game | occluder-mesh registration, characters/flyers | applies `min(terrain_vis, static_object_vis)` to materials |

**The seam (shared contract).** A minimal interface — fixed sun transform, world
bounds, the shared shadow-atlas/target handle, and the bake-ready signal. Put it in
a tiny `-core` crate both depend on (loosest coupling, static-shadows usable without
terrain), or have `bevy-static-sun-shadows` depend on `bevy-clipmap` directly
(simpler; the object bake wants terrain height anyway for placement + depth bias).
Prefer the `-core` contract for the more publishable factoring.

**Terrain-consistency in the object bake.** The object bake needs terrain for
*spatial* alignment, not as a caster: frame the bake camera over the terrain
footprint, and read terrain height to seat objects and set depth bias. It does
**not** need terrain baked into its depth map — terrain occlusion is added per
receiver via `min(terrain_vis, …)`, which is conservative and correct (a point in a
mountain's shadow is already 0 regardless of buildings). A ground-projected 2D
shadow texture would need full terrain *and* break for flyers — the depth-map
representation is why terrain is only needed for framing.

### 4.5 How static items receive shadows — the terrain shadow-height field

A static item (building, prop) receives two kinds of sun-shadow — from mountains
(terrain) and from other objects — and per the §4.4 contract both reduce to one
per-fragment term applied to the direct-sun term only (as the terrain does, §4):

    sun_vis(p) = min( terrain_vis(p), static_object_vis(p) )

Both halves are **live per-fragment samples of baked data** — the item's material
samples them; nothing is baked per-item.

**Terrain shadows on items — the shadow-height field (the load-bearing half).** A
naive top-down "is this XZ shadowed" texture fails on anything tall: a tower's top
and base share one XZ, so they'd read one value. Instead bake, per world column,
the **height of the top of the terrain's shadow** — the shadow *ceiling* `H(x,z)`.
A receiver fragment is terrain-shadowed by a height comparison:

    terrain_vis(p) = smoothstep(H(p.xz) - penumbra, H(p.xz), p.y)   // 1 tap + compare

A 2D texture but a **3D query** (the fragment brings its own `p.y`), so it does
partial shadows correctly — the whole point:

- **Tall tower, top-lit / base-shadowed.** Base fragments have `p.y < H` → shadow;
  higher fragments clear the mountain's shadow ceiling, `p.y > H` → sun; the line
  lands exactly at `p.y = H(x,z)`.
- **Lateral / diagonal split.** `H` varies across the item's footprint, so a shadow
  edge cutting across the walls is just the surface `p.y = H(x,z)` ∩ the geometry.

This is **exact** for a directional sun — parallel rays make a column's shadow
"everything below one ceiling," so one height per column has zero error (it is the
fixed-sun specialization of a horizon map, §4.1; store the *height* rather than a
per-azimuth angle so off-surface receivers can sample it). Soft edges come from the
`penumbra` band (fixed width — an approximation; real penumbra widens with occluder
distance). Edge sharpness is bounded by `H`'s texel size — bake at RVT/heightmap
resolution.

The same field is what the terrain surface and flyers query, so it **subsumes the
§4.3 per-entity march**: a flyer's terrain-vis becomes one `H` lookup instead of a
32–64-tap march, height-correct by construction. `H` is baked once (a heightfield
sweep in the sun's azimuth — a running max with a per-step drop, O(N), same class as
the self-shadow march) into one `R16Float`/`R32Float` world-XZ texture.

**Object shadows on items** — `static_object_vis` is the §4.4 baked sun-depth map /
SDF (buildings/trees casting, including self- and inter-object). A depth compare,
height-correct for arbitrary geometry (overhangs — which the height field can't
represent, hence two artifacts + `min()`, not one).

**Crate ↔ game boundary.** The item's material is a `MaterialExtension` (like the
existing `HeightFogExtension`) that (1) binds the crate's terrain shadow-height
field handle + world extents + penumbra (part of the shared contract, alongside the
fixed sun frame and `ClipmapReady`), (2) `#import`s the crate's `terrain_vis` WGSL,
(3) samples `min(terrain_vis(world_pos), static_object_vis(world_pos))` and
multiplies it onto the sun term. The crate owns the terrain field;
`bevy-static-sun-shadows` owns the object depth map; the game wires both into its
item materials. Single source of truth: the item reads the field the crate actually
baked, so it can't drift from the terrain's own shadow.

**Not covered here (deliberately).** This is the shadow *shape*, not the light that
*fills* it. A correctly-shaped shadow over flat ambient reads flat; realistic
shadowed faces need indirect fill — the GI pillar, §4.6.

### 4.6 Global illumination — baked at load, sampled cheap (the fill)

Shadows give the *shape*; GI gives the *fill* that keeps shadowed faces from reading
flat. This is the pillar that carries the AAA-VR look (it's why Alyx / Red Matter 2
feel grounded), and the one the baked-shadow work above does **not** provide. The
target is user-generated maps on standalone VR, so it must **bake at load** (no
offline lightmap step, no lightmap UVs) and cost near-nothing per frame.

**Governing split — bake static, sample dynamic.** Fixed sun + static geometry ⇒ the
indirect light a static surface receives is *constant*, so it is **baked into the
surface once** and never sampled per frame:

- **Terrain** — fold the received irradiance into the **RVT** (extends the existing
  bent-normal + macro-AO ambient gather, §3.4/§4.2, from monochrome sky-occlusion to
  a colored sky + single sun-bounce term). Runtime GI cost for terrain ≈ **free** —
  it's already sampling the RVT.
- **Static buildings** — bake received irradiance into **per-vertex colors** at load
  (UV-free, so it works on arbitrary user glbs — no lightmap unwrap). Runtime = one
  vertex-attribute read. Low-frequency GI is smooth, so per-vertex resolution is
  fine; the crisp detail comes from the *shadow* (§4.5), not the GI.
- **Dynamic objects** (players, flyers, projectiles) — the *only* consumers that
  sample the probe volume **live**, and **per-object** (a few lookups at the object's
  position), never per-fragment. Dozens of objects, not millions of fragments.

**The efficiency crux — never sample visibility-weighted probes per pixel.** Plain
irradiance volumes are cheap because hardware 3D-texture trilinear blends the 8
neighbor probes for free. The leak-fix visibility test (below) *defeats* that — it
forces 8 manual irradiance+depth fetches + a chebyshev weight per sample (~16 fetches
+ math). Full-screen on Adreno that's fatal. Baking it into static surfaces and
reserving live sampling for the few dynamic objects is what keeps it in budget. This
is the standard AAA-VR split (static baked, dynamics probe-sampled).

**Leak-free interiors (Tribes has indoors).** A uniform grid leaks outdoor light
through walls. The fix, with **no room authoring** (UGC-safe): **per-probe
visibility** — each probe stores depth/distance moments; a surface weights each
neighbor probe by whether it can *see* the surface (DDGI's chebyshev test). A probe
behind a wall fails → contributes ~0 → no bleed, while a probe that sees the surface
*through a doorway* correctly spills light in (visibility admits openings, blocks
walls — better than hard portal-sealing, which over-darkens near doors). Structure:

- **Nested volumes** — a coarse outdoor grid over the terrain + a **denser local
  volume per building glb** (sized to its AABB). Known bounds say *where* to densify;
  the visibility test decides *which* probes are interior (the AABB alone can't — it
  holds interior air, wall solid, and exterior air).
- **Probe relocation + classification** — nudge probes out of walls; deactivate ones
  sealed in solid.
- **glb walls are the occluders** — the same meshes registered as §4.4 shadow casters
  feed the GI visibility bake. Register a building once → it feeds crisp shadows *and*
  leak-blocking. **Interior lights bake in** too (any static lights present at bake
  time), so a base interior gets real interior lighting, not just absence-of-sun.

**Bake pipeline (reuses existing machinery).** A probe bakes by rendering a small
cubemap from its position (irradiance) + its depth (visibility moments) — the *same*
camera-to-texture pattern the RVT already uses. Multi-bounce = re-bake probes
sampling the previous pass's probes (cheap SH-grid iterations). Static surfaces then
gather from the baked probes into the RVT / per-vertex colors. All amortized at load
behind `ClipmapReady`, like the RVT bake.

**Frequency split (why it's cheap and looks right).** Low-frequency GI is baked at
low resolution (into the RVT / per-vertex); high-frequency shadow stays per-fragment
(§4.5). Direct term = `sun · sun_vis` (§4.5 shadow); indirect term = baked GI. Two
separate terms, no double-count, each stored at the resolution it needs.

**Risks — this is the biggest single subsystem here, and "efficient enough" is a
measured claim.** Where cost actually lives (all bake/VRAM, not frame):
- **Bake time** — per-building probe grids × cubemap renders + baking GI into every
  building's vertices, per map. A big map with many bases could be a long "building
  lighting…" load. Amortize over frames; interior probe density is the dial.
- **VRAM** — probe irradiance **+ depth moments** (visibility roughly doubles probe
  memory) + a **new RVT irradiance target** (all existing RVT channels are full →
  another sampled texture in `GridMaterial`, against the ~16/stage ceiling the code
  already manages — the exact class of change that once silently broke the pipeline).
- **Dynamic-object count** — Tribes throws many players/projectiles; keep GI
  per-object (throttle/cache slow movers), never per-fragment.
- **All custom** — Bevy gives the runtime voxel sampling (`IrradianceVolume`) but
  **not** leak-free visibility or any baker. The nested-volume + per-probe-visibility
  bake and sample are ours to build.

**Prototype before committing.** Bake a small probe volume with visibility, fold the
result into the RVT + one building's vertices, sample it on a moving object, and
**profile on the Frame** — the architecture is the right shape, but the on-device
number is the thing to earn before building it out. Composes with (does not replace)
the §4.5 shadow work, and stays within the §4.2 no-realtime rule (all baked, fixed
sun).

---

## 5. Lighting & shadows — dynamic tier (SUPERSEDED by §4.2)

> **Superseded.** The realtime techniques below (CSM, live ray-march, lazy
> refresh) are **not** pursued even as a high-power tier — §4.2's no-realtime
> rule stands for every target. Kept as a record of the evaluation. A moving sun
> under the current rule would mean **re-baking** the sun-visibility channel
> (lazily, budgeted), not live shadowing.

The original evaluation follows. The AAA VR-viable answer for a dynamic sun was a
**hybrid**, merged into one `sun_visibility` scalar:

| Range / caster                | Technique                                             | Status |
| ----------------------------- | ----------------------------------------------------- | ------ |
| Near/mid dynamic objects      | Cascaded Shadow Maps (Bevy, have it) + contact shadows (Bevy 0.19) | available |
| Terrain self-shadow, all dist | **Ray-marched heightfield** (maximum-mipmap of the heightmap, arbitrary sun) | build later |
| Far *object* shadows          | Distance Field (SDF) ray-march                        | build later, optional |

VR optimization: even with a moving sun, most shadow change is slow. Keep the
RVT sun-visibility channel but **refresh it lazily** (every N frames as the sun
creeps) for the slow parts (terrain self-shadow + static-object occlusion at the
current angle); only genuinely dynamic objects use live CSM every frame.

**Rejected for VR-first:**
- **Virtual Shadow Maps (VSM)** — Unreal's flagship dynamic-sun system; ~9ms
  shadow pass on console even after UE5.7 improvements, cache invalidation from
  head motion + stereo, and not present in Bevy. Known AAA reference, wrong tool
  here.
- **Hardware ray-traced / path-traced shadows** — highest quality, but
  perf-hostile for mainstream VR; Bevy's Solari is experimental.

---

## 6. Foundational cleanup (do before piling on features)

### 6.1 Shrink or eliminate the copied lighting shader
`terrain_apply_lighting` in `terrain.wgsl` is a hand-copied fork of Bevy's
`apply_pbr_lighting`, now existing to inject `sun_vis` onto **only the direct sun
term** (the horizon-map reason is gone — §4.1). It is the single biggest
maintenance liability (re-taxed every Bevy upgrade — already felt in the 0.18→0.19
jump).

- The directional loop is the only place the fork *needs* to diverge (the
  `sun_vis` multiply). Everything else — the clustered **point/spot light loops**
  (ported verbatim from `pbr_functions.wgsl` so terrain receives local lights),
  env map, ambient — is a straight copy of stock and carries no terrain-specific
  logic, so it re-syncs mechanically but must be re-checked each Bevy bump for
  signature drift (`point_light`/`spot_light`/`fetch_*_shadow` args, cluster range
  offsets). The dropped-for-terrain parts stay dropped: clearcoat, transmission,
  anisotropy, contact shadows, lightmap.
- Ideal end state: apply the baked shadowmask another way (e.g. a per-pixel
  shadow factor fed to stock `apply_pbr_lighting`) so the fork disappears entirely
  and the point/spot loops come for free from upstream.
- Whatever remains: mark the divergence with a loud banner, keep it minimal, and
  maintain a `RESYNC.md` re-sync checklist. Pin the exact Bevy minor.

### 6.2 Cargo / version hygiene
`Cargo.toml` declares `bevy = "0.19.0"`, but the README compat table lists
0.18/0.17. Reconcile.

---

## 7. Quality scaling — VR vs flatscreen

Mechanism: a **`TerrainQualityKey`** driving pipeline specialization + shader
defs, generalizing the existing `WireframeKey` / `specialize()` pattern
(`src/lib.rs:463-539`), plus **crate feature flags** (`hextile`, `triplanar`,
`rvt`, `parallax`) so the heavy pieces are opt-in at the library level. That
feature-flag layout *is* the "optional increased visuals" deliverable.

Because RVT decouples per-pixel cost from material complexity, the **material
graph is identical across tiers** — the quality knob is mostly *RVT texel
density + bake frequency + AA + shadow resolution + near-ring effects*, not a
material rewrite.

| Knob                     | VR preset            | Flatscreen "ultra"      |
| ------------------------ | -------------------- | ----------------------- |
| RVT resolution           | 4096² (measure!)     | 8192²                   |
| toroidal ring RVT (§2.1) | off (static bake)    | optional near ring      |
| triplanar                | slopes only / off    | slopes on               |
| detail-texture distance  | short                | long                    |
| parallax / POM           | off                  | on                      |
| baked shadow/AO channels | sun-vis only         | + object AO (§4.2)      |
| anti-aliasing            | MSAA (forward)       | TAA/DLSS-class          |

---

## 8. VR correctness & longer-horizon items

- **Center the clipmap on the HMD head, not an eye.** `update_grids` follows a
  single `clipmap.target` entity (`src/lib.rs:420`). In stereo, both eyes share
  one head — point `target` at the head/tracking-space origin so both eyes share
  one clipmap and one RVT.
- **Verify multiview-safety** once stereo is added (via a community OpenXR crate;
  no first-party Bevy XR in 0.19). Per-material uniforms like `translation` are
  view-independent — good.
- **Forward + MSAA for the VR path; keep deferred for flatscreen.** Both
  pipelines already exist (`deferred_output` in `terrain.wgsl`). Treat the choice
  as part of the quality preset.
- **Stable shadows.** Snap CSM texels to world grid; prefer stable techniques —
  shadow shimmer is nausea-inducing in a headset.
- **Later — stream source data.** RVT caches *shading*; source heightmap/color
  are still single resident images. For planet-scale worlds, stream tiled
  heightmap + material IDs feeding the RVT bake. Keep the `TerrainLayerSet` asset
  and bake pass agnostic to whether inputs are resident or streamed.

---

## 9. Build order

**Done** — detail in the referenced sections. Horizon-map removal + lighting-fork
shrink (§4.1, §6.1); procedural splat blending with per-layer normal/ORM arrays and
slope/height placement bands (§3.1–3.2); the static bake-once RVT, ~14 samples → ~2
(§2, §3.4); the near-range detail overlay through v3 (top-2 material blend +
far-field skip, §3.5); baked hex de-tiling (§3.3); baked terrain self-shadow via a
fixed-sun heightfield ray-march (§4); the sentinel-gated bake lifecycle (§2);
`load_terrain_array` + real CC0 textures (§3.6); RVT at 8192²/~1 m-texel; macro-map
removal. The toroidal ring RVT and the decoupled sky-AO channel were built and
**shelved** (§2.1, §4.2).

**Next:**
- **Per-entity sun-visibility query** (§4.3, priority) — the plugin exposes
  `sample_sun_visibility(world_pos)` (CPU + WGSL); the game applies it to flyers.
- **Baked static-object shadows** (§4.4) — a separate `bevy-static-sun-shadows`
  crate, combined via `min(terrain_vis, static_object_vis)`; also revives the
  sky/contact-AO channel (§4.2). Dynamic objects stay blob/decal fakes.
- **Quality scaling into presets** (§7).
- **(Stretch)** triplanar on steep slopes (§3.3); the toroidal ring RVT for a
  flatscreen near-density tier (§2.1).

**Known follow-ups:** RVT edge-stripe smear past heightmap coverage (clamp
sun-visibility / fade outside coverage); on-device Quest profiling of the 8192²
build (VRAM vs 4096² — decide from a headset); optionally pull `nor_dx` textures to
drop the in-shader X-flip.

**Quest validation gate (measure on-device, not on desktop).** The architecture is
the right *shape* for the Quest — zero per-frame shadow passes; the whole
static-object system is ~1 sample/fragment — but "efficient enough" is a measured
claim (§2.1, §7). Before committing presets, profile on a headset:
- **RVT resolution / VRAM** — 8192² is ~512 MB of 6 GB (Quest 2) / 8 GB (Quest 3)
  shared; the VR preset likely drops to **4096²** (§7). Decide from the headset.
- **Samples per near fragment** — tally RVT albedo + normal + detail (×2 top-2
  blend) + the static-object shadow sample against the ~16/stage ceiling *and*
  bandwidth. Detail is already far-skipped; measure the worst near case.
- **Per-entity march (§4.3)** — must run once-per-entity off a **resident CPU
  height array** (or a tiny compute), never per-fragment, never a per-frame
  GPU→CPU readback.
- **Trees** — SDF/proxy baked, **never traced per-frame** (§4.4).
- **Target device** — Quest 2 (weaker Adreno, 6 GB) vs Quest 3/3S (8 GB) changes
  the presets; be conservative if Quest 2 is in scope.

---

## 10. Key code anchor points (current)

| Location                                    | Role                                                    |
| ------------------------------------------- | ------------------------------------------------------- |
| `src/bake.wgsl` `splat_terrain` / `hex_sample` | Layer splat + hex de-tiling; writes the RVT channels |
| `src/bake.wgsl` `band()`                    | Procedural slope/height placement bands (§3.2)          |
| `src/bake.wgsl` `sun_visibility`            | Adaptive heightfield ray-march (baked terrain shadow)   |
| `src/terrain.wgsl` `terrain_apply_lighting` | Compact PBR fork: baked sun-vis on the sun + clustered point/spot lights |
| `src/terrain.wgsl` packed-id unpack + detail block | Top-2 detail blend, NEAREST id read, far skip (§3.5) |
| `src/lib.rs` `load_terrain_array`           | Decode + stack + mip real texture sets (public API)     |
| `src/rvt.rs` `init_rvt` / `drive_rvt_bake`  | Sentinel-gated RVT bake lifecycle                       |
| `src/lib.rs` `TerrainLayer` / `SlopeRule` / `HeightRule` | Layer config + placement bands (§3.2)      |
| `src/lib.rs` `WireframeKey` / `specialize()`  | Pattern to extend for `TerrainQualityKey`             |
| `src/lib.rs` `update_grids` / `clipmap.target` | Clipmap follow; center on HMD head for stereo        |

---

## 11. References

- [Runtime Virtual Texturing in Unreal Engine](https://dev.epicgames.com/documentation/unreal-engine/runtime-virtual-texturing-in-unreal-engine?lang=en-US)
- [GPU Geometry Clipmaps (NVIDIA GPU Gems 2)](https://developer.nvidia.com/gpugems/gpugems2/part-i-geometric-complexity/chapter-2-terrain-rendering-using-gpu-based-geometry)
- [Terrain shader generation deep-dive (80.lv)](https://80.lv/articles/using-next-gen-terrain-engines-for-games-production-009snw)
- [Hex-tiling demo (Mikkelsen)](https://github.com/mmikk/hextile-demo)
- [Fun with horizon maps](https://dasilvagf.github.io/posts/2020/08/fun-with-horizon-maps/)
- [Maximum-mipmap terrain shadows (arXiv)](https://arxiv.org/pdf/2005.06671)
- [UE Distance Field Shadows](https://dev.epicgames.com/documentation/en-us/unreal-engine/using-distance-field-shadows-in-unreal-engine)
- [UE Virtual Shadow Maps](https://dev.epicgames.com/documentation/en-us/unreal-engine/virtual-shadow-maps-in-unreal-engine)
- [Bevy 0.19 release notes](https://bevy.org/news/bevy-0-19/)
- [bevy_mesh_terrain (splat + texture array)](https://github.com/ethereumdegen/bevy_mesh_terrain)
- [bevy_triplanar_splatting](https://github.com/bonsairobo/bevy_triplanar_splatting)
