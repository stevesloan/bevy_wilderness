# bevy_wilderness — Renderer Architecture

How the terrain renderer (`crates/bevy_wilderness`) works today. Read this before
changing the renderer; read [roadmap.md](roadmap.md) for where it is going and
[editor.md](editor.md) for the editor crates built on top of it.

Paths below are relative to `crates/bevy_wilderness/src/`. Function and type
names are given instead of line numbers so this stays valid as code moves.

---

## 1. The big idea

Terrain is expensive in two places — geometry (millions of triangles to the
horizon) and shading (blending many material layers per pixel). The renderer
attacks each separately:

- **Geometry: GPU geometry clipmaps.** A handful of shared flat grid meshes are
  arranged in nested rings around the camera, each ring twice the cell size of
  the one inside it. The vertex shader displaces them from a heightmap every
  frame, so there is no mesh generation, and editing the heightmap deforms the
  terrain on the next frame.
- **Shading: a baked, bake-once Runtime Virtual Texture (RVT).** Material
  splatting, terrain self-shadow, ambient occlusion and bent normals are
  rendered **once** into full-terrain textures. The per-frame pass reads ~2
  texels instead of blending layers, so shading cost is independent of
  material complexity. Close-up detail comes from a separate, distance-faded
  **detail overlay**, not from RVT density.

The governing performance rule: **anything static is baked once; the per-frame
path is a few texture fetches plus lighting.** This is what lets one material
scale from a standalone headset to a desktop GPU (see roadmap §2).

## 2. Module map

| File | Role |
| ---- | ---- |
| `lib.rs` | `ClipmapPlugin` wiring, public re-exports |
| `clipmap.rs` | `Clipmap` component, ring spawning/following, RVT target creation, PNG retag, clay-mode shadows (`editing`) |
| `mesh.rs` | Builds the five shared clipmap part meshes |
| `material.rs` | `GridMaterial` (the terrain material extension), pipeline key, `specialize` |
| `terrain.wesl` | Main pass: vertex displacement, holes, stamp preview; fragment shading + lighting fork |
| `rvt.rs` / `bake.wesl` | Bake material, sentinel-gated bake lifecycle, re-bake |
| `quality.rs` | `TerrainQuality`, `FogTier`, `TerrainFog`, `InlineFog` |
| `height_fog.rs` / `.wesl`, `fog_functions.wesl`, `mesh_fog.rs` / `.wesl` | Height fog: shared math, fullscreen pass, inline mesh fog |
| `hex_tiling.wesl` | Stochastic hex tiling, shared by the bake and the detail overlay |
| `sun_shadow.rs` / `.wesl` | Baked sun shadow-ceiling field for *your* meshes |
| `holes.rs` | Terrain holes (tunnels, caves, basements) |
| `heightfield.rs` | CPU height queries |
| `texture.rs` | `load_terrain_array`, samplers |
| `dev_controls.rs` | Debug keybinds (`dev-controls` feature) |

## 3. Public API

**Plugins.** `ClipmapPlugin` (everything terrain), `HeightFogPlugin`
(fullscreen fog post-process; add it yourself for `FogTier::High`).

**The terrain.** Spawn an entity with `Clipmap`:

- Geometry: `half_width`, `levels`, `base_scale` (level-0 cell size, m),
  `texel_size` (heightmap m/texel), `target` (entity the rings follow),
  `min`/`max` (the R16 encode range in meters), `looping`, `holes`, `wireframe`.
- Textures: `heightmap` (R16Unorm, or a 16-bit PNG — see §9), and the layer
  arrays `albedo_array` (alpha = height, used for height blending),
  `normal_array` (OpenGL convention), `orm_array` (occlusion, roughness,
  metallic).
- Material: `layers: Vec<TerrainLayer>` (≤ `MAX_TERRAIN_LAYERS` = 4) and
  `detail: DetailConfig`.
- `editing` only: `edit_overlay`, `stamp`, `clay`.

`ClipmapReady` is inserted on the entity when the bake finishes.
`TerrainSunShadow` is inserted for game materials (§7).

**Resources.** `TerrainQuality` (performance profile, §8), `TerrainFog` (the
fog *look*), `InlineFog` (active inline-fog params for your own shaders).

**Other.** `TerrainLayer`, `SlopeRule`, `HeightRule`, `DetailConfig`,
`HoleShape`, `MAX_HOLES` (24), `HeightFog`/`HeightFogParams`,
`HeightFogExtension`, `FogTier`, `SunShadowParams`, `load_terrain_array`,
`build_terrain_array`.

**Features.**
- `editing` — the editable-terrain API: `RebakeRequested`, `Heightfield`,
  `ClipmapStamp`, the overlay/stamp bindings, clay mode. Default consumers pay
  nothing for it.
- `dev-controls` — V cycles debug views (AO / bent normal / cavity),
  B / N toggle AO / bent-normal strength, H toggles hex-tiled detail.

## 4. Geometry — clipmap rings

`init_clipmaps` spawns `levels` child `ClipmapGrid` entities;
`build_clipmap_parts` builds five meshes shared by every level:

- `square` — the 4×4 tiles that make up a ring (inner 2×2 skipped above level 0)
- `center` — level 0's filler cross; `filler` — the cross gaps on outer levels
- `trim` — the L-shaped strip that absorbs the odd snap offset
- `stitch` — a perimeter of triangle fans joining a ring to the finer ring
  inside it, so there are no T-junction cracks

Level *n* is scaled by `base_scale · 2ⁿ` (the cell size in meters).
`update_grids` snaps each level to its own grid (`floor(target / snap) · snap`)
and rotates the trim by snap parity; transforms are only written when a snap
boundary is crossed. All parts are `NotShadowCaster` with hand-set AABBs whose Y
covers `min..max`.

**Vertex shader** (`terrain.wesl::vertex`): world XZ → heightmap UV
(`xz / world_size + 0.5`, terrain centered on the origin) → manual bilinear of
four `textureLoad`s (`height_bilinear`) → `min + h · (max − min)`.

- **Finite terrain** clamps the UV; vertices past the heightmap coverage drop
  to `min` so the coarse rings don't render a wall at the edge.
- **Looping terrain** wraps texel indices toroidally (flags bit 3); the
  heightmap tiles infinitely with no seam.
- **Holes** (`WILDERNESS_HOLES`, vertex stage only): a vertex inside a hole
  shape gets a NaN clip position, which culls its triangles before
  rasterization (no `discard`, so early-Z survives). A shape only cuts rings
  whose cell size is ≤ its extent / 3 (`HOLE_MIN_CELLS`), so small holes seal
  at distance. Tests use world XZ + instance scale, so both stereo eyes agree.
  Holes are visual only — cut your own collider.
- **Stamp preview** (`editing`): see [editor.md](editor.md) §5.

LOD transitions are **not morphed**: rings switch resolution at snap
boundaries (see roadmap: geomorphing).

## 5. Material — `GridMaterial`

The terrain material is `ExtendedMaterial<StandardMaterial, GridMaterial>`;
one solid + one wireframe instance per clipmap, shared by all rings.

| Binding | Contents |
| ------- | -------- |
| 102 / 103 | heightmap / its sampler (103 is shared by overlay and stamp) |
| 108 | `texel_size` |
| 109 | `minmax` |
| 110 | `DevParams`: AO/bent strength, debug view, stamp transform, `HoleBuffer` (48 × vec4), `hole_count` |
| 111 | `flags` (u32, below) |
| 114 | inline `HeightFogParams` |
| 115 / 116 | `editing`: edit overlay, stamp heightfield |
| 121 / 122 | RVT albedo / sampler shared by all RVT targets |
| 123 | RVT normal |
| 125 / 126 / 127 / 130 | detail albedo / sampler / normal / ORM arrays |
| 129 | `DetailParams` |
| 132 | RVT AO |

`flags`: bit 0 wireframe · 1 ambient gather on · 2 single-layer detail ·
3 looping · 4 clay · 5 stamp preview · 6 stamp mask-weighted · 7 hex-tiled
detail (pipeline key only).

> ⚠️ **Binding ceiling.** The material sits at the per-stage limit for uniform
> buffers and near the ~16 sampled-texture limit of mobile GPUs. Adding uniform
> bindings once broke the pipeline **silently** (terrain simply vanished). New
> scalars go into `flags` or grow `DevParams`; new textures share samplers.
> Bindings 112/113 are deliberately free headroom.

**Shader defs pushed in `GridMaterial::specialize`:**
- `WILDERNESS_SHADE` — every pipeline that shades (forward, deferred g-buffer).
  Depth-only pipelines (shadow, depth prepass) compile `fn vertex` alone:
  `pbr_fragment` doesn't build against the prepass `VertexOutput`, and their
  layouts lack light bindings. The prepass vertex shader is `terrain.wesl` too,
  so shadow casters are displaced.
- `WILDERNESS_HOLES` — `GridMaterialKey.holes` (any holes present).
- `WILDERNESS_DETAIL_HEX` — fragment; `TerrainQuality::detail_hex_tiling`
  (flags bit 7). Compiles the hex path in; cheaper tiers carry none of it.
- `WILDERNESS_EDIT_OVERLAY` / `WILDERNESS_STAMP` — `editing` builds.

## 6. The bake — RVT and friends

### 6.1 Targets

| Target | Format | Size | Channels |
| ------ | ------ | ---- | -------- |
| albedo | `Rgba8UnormSrgb` | `rvt_size`² | rgb albedo · a sun visibility |
| normal | `Rgba8Unorm` | `rvt_size`² | rg octahedral world normal · b roughness · a packed material ids |
| ao | `Rgba8Unorm` | `rvt_size`² (4×4 stub when gather is off) | r macro AO · gb bent normal XZ · a cavity |
| sun shadow | `Rgba16Float` | `sun_shadow_size`² | r shadow ceiling (biased) · g distance to occluder |

Targets have a **single mip level** (see roadmap: RVT mips). Looping terrain
samples them with a Repeat sampler.

### 6.2 What each bake computes (`bake.wesl`)

All bake modes read the heightmap the same way as the vertex shader, wrapping
on looping terrain so marches cross the seam.

- **Splat** (`splat_terrain`). Each layer's weight is the product of two
  optional bands — `SlopeRule` (degrees) and `HeightRule` (meters) — each a
  smoothstep ramp of width `blend` outside `[min, max]`. A layer with no rules
  is present everywhere (the "base layer" pattern, which keeps boundaries
  organic). Weights then go through **height blending**: each layer scores
  `weight × (1 ± height_blend)` modulated by its height (albedo alpha), and
  layers within 20 % of the best score contribute — so grass fills the cracks
  between rocks instead of cross-fading. The height term *scales* the weight
  (rather than adding to it) so a layer thins out continuously at its band
  edge; an additive term drew a hard line around every band. Every layer
  is sampled with **hex (stochastic) tiling** (Mikkelsen, 3 taps) to kill
  repetition — affordable because it runs in the bake, not per frame.
  Normals are reoriented onto the geometric normal (central differences).
  The **top two layers** and their blend are packed into the normal target's
  alpha byte (`i0 + 4·i1 + 16·q`) for the detail overlay.
- **Sun visibility** (`sun_visibility`, albedo alpha). Soft heightfield
  ray-march toward the sun: 96 geometrically growing steps out to 6 km, origin
  pushed off the surface, penumbra from clearance / distance.
- **Ambient gather** (`ambient_gather`, AO target). Horizon-based over 8
  azimuths × 12 steps out to 400 m: macro AO integrated about the surface
  normal (an unoccluded slope bakes to 1), the visibility-weighted **bent
  normal**, and a Laplacian **cavity** term. Off on low quality.
- **Shadow ceiling** (`shadow_ceiling`, sun-shadow target). For each column,
  the height below which the terrain shadows it (and the occluder distance for
  penumbra width) — §7.

### 6.3 Lifecycle (`rvt.rs`)

`init_rvt` waits for the heightmap **and** a `DirectionalLight`; the bake's sun
is that light's `back()`, so the bake can never disagree with the scene's sun.
For each target it spawns an inactive top-down orthographic camera (up = −Z, so
texel layout matches `xz / world_size + 0.5`) over a full-terrain quad on its
own render layer.

**Sentinel gating.** A camera that renders before its pipeline compiles and
textures are resident bakes black, and an unbaked terrain reads as a chrome
mirror. So each target first draws to a 4×4 sentinel that is read back every
frame; the first non-black readback proves the pipeline is ready, the real
bake camera runs for 2 frames (`RVT_BAKE_FRAMES`), and everything despawns.
When all targets land: `ClipmapReady` is inserted, `TerrainSunShadow` is marked
valid, `ever_baked` latches.

**Re-bake** (`editing`). Insert `RebakeRequested`; `process_rebake_requests`
re-runs the lifecycle, resizing targets in place if bake-time quality changed.
Old shading stays on screen until the new bake lands.

## 7. Shadows

There are **no realtime shadow passes in normal rendering** — terrain is
`NotShadowCaster`, and terrain shadowing is baked against a fixed sun:

- **On the terrain:** baked sun visibility (RVT albedo alpha) multiplies only
  the direct sun term, so shadowed valleys keep sky light.
- **On your meshes — the shadow-ceiling field.** `TerrainSunShadow.field`
  stores, per world column, the height of the terrain's shadow ceiling. A
  fragment at height `y` is lit when `y` clears the ceiling, so the query is
  **height-correct** for anything: building walls, characters, flyers. Bind
  the field in your material, copy `params` into a uniform, and call
  `bevy_wilderness::sun_shadow::sun_visibility(field, sampler, params, pos)`.
  Penumbra widens with occluder distance (the sun's 0.53° disc). Unbaked or
  out-of-bounds reads as full sun.
- **Clay mode** (`editing`, below) is the one place CSM is used.

## 8. Main pass shading (`terrain.wesl::fragment`)

1. **RVT fetch.** Albedo + sun-vis, octahedral normal + roughness, and (if the
   ambient gather is on) AO + bent normal.
2. **Detail overlay**, faded by camera distance (`DetailConfig.near/far`).
   Skipped entirely past `far` (derivatives hoisted out of the branch). Reads
   the packed material ids **NEAREST** (a packed byte can't be filtered) with a
   world-locked ±½-texel jitter standing in for bilinear, then samples the top
   material's detail albedo / normal / ORM — and the second material's, unless
   single-layer detail is set or one material dominates. The two are combined
   with a **per-pixel height blend** that mirrors the bake's (detail heights
   from albedo alpha × each layer's `height_blend`), so the second material
   shows in the low spots of the first — grass in the cracks between stones —
   instead of as a cross-fade. The blend drives **colour** too: the RVT colour
   divided by its texel's mean material mix gives the macro tint, which times
   the per-pixel mix gives each pixel its own material's colour (the RVT alone
   is a ~1 m bilinear smear across boundaries). Detail albedo
   contributes a **unit-mean ratio** (`da / da_mean`, the mean read from the
   coarsest mip) so it adds variation without double-darkening the RVT albedo.
3. **Lighting** — `terrain_apply_lighting`, a trimmed fork of Bevy's
   `apply_pbr_lighting` (base layer only):
   - directional lights × CSM shadow (if any) × **baked sun-vis**, with grazing
     specular suppressed on rough ground (dry terrain shouldn't look wet at
     sunset);
   - clustered point and spot lights with their own shadows (not sun-vis);
   - **micro-shadowing** on the sun from the detail AO (Uncharted 4's
     approximation, `saturate(|N·L| + 2·ao² − 1)`), so crevices read as
     recessed under direct light, not only in ambient;
   - environment map diffuse looked up along the **bent normal**, with colored
     multi-bounce AO and specular occlusion from AO. Macro AO includes the
     baked **cavity** term (concave ground loses up to 60 % of its ambient);
   - flat ambient; inline height fog when on the Low fog tier.
   This fork exists because Bevy has no hook for a per-pixel factor on the
   directional term only. **Re-check it on every Bevy upgrade** against
   `pbr_functions.wesl` (0.20 inserted rect area lights into the cluster
   ranges; terrain does not receive rect lights).
4. Deferred builds write `deferred_output` instead of lighting.

Debug views (`dev-controls`): 1 macro AO, 2 bent normal, 3 cavity — raw,
unlit.

**Clay mode** (`editing`). While shading is stale (`clipmap.clay`, or never
baked) the fragment ignores the RVT and renders grey clay with derivative
normals. To keep terrain readable, `sync_clay_shadows` turns on terrain-scale
CSM and makes the terrain cast. ⚠️ This relies on a Bevy internal: opaque
shadow casters render through a depth-only path without the material bind
group, so the solid material is switched to `AlphaMode::Mask(0.0)` (nothing
discards; it just routes through the material-bound path). Everything is
restored on exit. Re-verify on every Bevy upgrade.

## 9. Quality, fog and supporting systems

**`TerrainQuality`** (resource). Defaults are desktop-grade; dial down for
standalone VR.

| Field | Default | Effect | Applies |
| ----- | ------- | ------ | ------- |
| `fog` | `High` | fullscreen fog pass vs inline fog | live |
| `rvt_size` | 8192 | albedo / normal / AO target size | spawn or re-bake |
| `ambient_gather` | true | bake + sample AO / bent normal | spawn or re-bake |
| `detail_layers` | 2 | 1 = skip the second material's detail samples (and the per-pixel height blend) | spawn or re-bake |
| `detail_hex_tiling` | true | hex-tile the detail overlay (3× near detail samples) | live |
| `sun_shadow_size` | 2048 | shadow-ceiling field size | spawn or re-bake |

Bake-time fields are snapshotted when the clipmap spawns; changing them later
warns unless a re-bake is queued.

**Height fog.** One analytic exponential height-fog function
(`fog_functions.wesl`) feeds two tiers so flatscreen and VR match:
- `FogTier::High` — `HeightFogPlugin` fullscreen pass, reconstructs position
  from depth, fogs the sky too. Needs `Msaa::Off`.
- `FogTier::Low` — inline in the terrain and in meshes using
  `HeightFogExtension` (or your own shader via `InlineFog`). MSAA-friendly,
  nearly free. Set `HeightFog.max_distance` = camera `far`.

**Heightmaps.** R16Unorm KTX2 is the master format. 16-bit PNGs decode as
`R16Uint`; `retag_png_heightmaps` relabels them `R16Unorm` in place (identical
bytes) before anything reads them.

**Textures.** `load_terrain_array(images, paths, srgb)` decodes one file per
layer into a mip-chained 2D array (mips matter: the bake samples heavily
minified). **Albedo alpha must hold the layer's height** (0–1, full range):
both the bake's and the overlay's height blending read it; opaque alpha
silently turns height blending into a plain contrast cross-fade.
`assets/fetch_textures.py` packs Poly Haven displacement maps there. Normal maps are **OpenGL convention**; the bake and overlay flip X
to match the reorientation tangent — DirectX normals make cracks look raised.

**`Heightfield`** (`editing`): CPU bilinear height query matching the shader.
Requires a CPU-resident R16Unorm image; does not wrap for looping terrain.

## 10. Per-frame cost

| When | Cost |
| ---- | ---- |
| Every frame, CPU | ring snapping (writes only on snap), change-gated quality/fog/holes sync |
| Every frame, GPU vertex | 4 heightmap loads (+ hole loop, + stamp in editing) |
| Every frame, GPU fragment, far | 2–3 RVT samples + lighting |
| Every frame, GPU fragment, near | + 1 id load + 4 detail samples (8 when blending two materials); ×3 for the hex-tiled taps when `detail_hex_tiling` is on |
| Every frame, High fog | one fullscreen pass |
| Once per bake | 4 full-terrain render passes (hex-tiled splat ×2, gather, shadow ceiling), ~2 frames each |

## 11. Invariants and gotchas

- **Terrain is origin-centered**; world size = heightmap texels × `texel_size`.
- **CPU ↔ GPU twins must stay identical:** `HoleBuffer::pack` ↔ the shader
  unpack; `DevParams` ↔ the WGSL struct (field for field); `Heightfield` ↔
  `terrain_height`; the stamp preview ↔ the editor's commit.
- **Avoid needless asset mutation.** `get_mut` on an image or material marks it
  changed (re-upload / bind-group rebuild); every sync checks before writing.
- **Material ids are read NEAREST** — filtering a packed byte bands.
- **Don't jitter splat bands with value noise** — thresholding lattice noise
  makes every material edge follow square cells. Use gradient noise if ever.
- **Hex cells must span ~1 texture repeat** — smaller collapses to speckle in
  the minified bake, larger shows repetition.
- **Bake camera up is −Z**; `+Z` rotates every baked channel 180°.
- Holes, the shadow field and fog are view-independent — safe for stereo.

## 12. Tried and rejected

Recorded so they aren't rebuilt without new information:

- **Camera-centered toroidal RVT ring** (1 km, 0.25 m/texel, double-buffered,
  amortized): worked, shelved — per-frame re-bake work competes with the VR
  frame budget, and the 8K static RVT was good enough.
- **FFT horizon map** for terrain shadow: removed — redundant with a fixed sun.
- **Sky-occlusion channel** in a separate shadow texture: nearly invisible on
  open terrain; superseded by the ambient gather.
- **Painted control map** and **macro variation map**: removed — placement is
  procedural (user-generated terrain), and hex tiling + 8K RVT carry vistas.
- **Erosion** (CPU droplets, then a GPU pipe model): removed from the editor —
  never produced convincing results. See roadmap for the replacement plan.
