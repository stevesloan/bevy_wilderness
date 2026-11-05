# bevy_wilderness — Renderer Architecture

How the terrain renderer (`crates/bevy_wilderness`) works today, and the work
left open in it (§13). Read this before changing the renderer, and
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
- **Shading: a texture clipmap.** Material splatting and cavity are composited
  into a texture array, one layer per level, each level following the target at
  half the density of the one inside it, the last covering the whole world. Only
  the strips entering a level as the target moves are composited, so a still
  camera composites nothing. The per-frame pass reads one or two levels instead
  of blending layers, so shading cost is independent of material complexity.
  Close-up detail comes from a separate, distance-faded **detail overlay**.
  The sun shadow-ceiling field is the one whole-world target still baked at
  load.

The governing performance rule: **anything static is composited once, where the
camera first needs it; the per-frame path is a few texture fetches plus
lighting.** This is what lets one material scale from a standalone headset to a
desktop GPU: tiers differ in the clipmap's density and budget, not in the
material.

## 2. Module map

| File | Role |
| ---- | ---- |
| `lib.rs` | `ClipmapPlugin` wiring, public re-exports |
| `clipmap.rs` | `Clipmap` component, ring spawning/following, PNG retag, clay-mode shadows (`editing`) |
| `mesh.rs` | Builds the five shared clipmap part meshes |
| `material.rs` | `GridMaterial` (the terrain material extension), pipeline key, `specialize` |
| `terrain.wesl` | Main pass: vertex displacement, holes, stamp preview; fragment shading + lighting fork |
| `vt.rs` | Texture clipmap, main world: level layout, windows, composite requests, visible rects |
| `vt_render.rs` / `vt_composite.wesl` | Texture clipmap, render world: the composite pass |
| `rvt.rs` / `bake.wesl` | Sun shadow field bake: sentinel-gated lifecycle, re-bake |
| `quality.rs` | `TerrainQuality`, `TerrainFog`, `InlineFog` |
| `height_fog.rs`, `fog_functions.wesl` | Height fog: its parameters and shared math |
| `sun_shadow.rs` / `.wesl` | Baked sun shadow-ceiling field for *your* meshes |
| `holes.rs` | Terrain holes (tunnels, caves, basements) |
| `heightfield.rs` | CPU height queries |
| `texture.rs` | `load_terrain_array`, samplers |
| `dev_controls.rs` | Debug keybinds (`dev-controls` feature) |

## 3. Public API

**Plugins.** `ClipmapPlugin` (everything terrain, height fog included).

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

`ClipmapReady` is inserted on the entity once the shadow field has baked and
every clipmap level is composited.
`TerrainSunShadow` is inserted for game materials (§7).

**Baked lighting (optional).** Insert a `TerrainBakedLight` on the clipmap to
light it from an offline bake the host made: one `Rgba16Float` atlas and up to
`MAX_BAKED_LIGHT_REGIONS` (8) `BakedLightRegion`s, the first covering the whole
terrain and the rest denser patches (the last covering a point wins, faded in
over `patch_fade` at its edge). Each region is two tiles: colour (RGB
irradiance / π in scene light units, A sun visibility) and the dominant
direction (RGB `d·0.5+0.5`, A the rebalance against the baked normal). While
present it replaces the environment map's diffuse, the flat ambient and the
field's ground shadow (§8); remove it when the terrain no longer matches the
bake (an edit) and the terrain lights itself again. The crate never bakes it.

**Resources.** `TerrainQuality` (performance profile, §9), `TerrainFog` (the
fog *look*), `InlineFog` (the fog's params for your own shaders),
`TerrainDebugView` (what the terrain's fragment shader outputs, §8).

**Other.** `TerrainLayer`, `SlopeRule`, `HeightRule`, `CavityRule`, `DetailConfig`,
`HoleShape`, `MAX_HOLES` (24), `HeightFog`/`HeightFogParams`, `SunShadowParams`,
`load_terrain_array`, `build_terrain_array`.

**Features.**
- `editing` — the editable-terrain API: `RebakeRequested`, `Heightfield`,
  `ClipmapStamp`, the overlay/stamp bindings, clay mode. Default consumers pay
  nothing for it.
- `dev-controls` — V cycles `TerrainDebugView` through lit, cavity, sun
  visibility and flat, and the clipmap logs its composite cost once a second
  while compositing (§10).

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

**Geomorphing.** Each ring has half the resolution of the one inside it, and
rings snap with the camera, so an unblended seam would jump as you move. Across
the outer 30 % of each ring (`MORPH_BAND`, by Chebyshev distance from the
camera) every vertex blends toward the height the coarser ring renders there:
the same height on shared vertices, the average of a coarse edge's ends, or
the coarse quad's (+x, +z) diagonal at its centre (`coarse_height_at`). At the
seam the two rings match exactly. The band finishes 4 cells inside the ring
edge to cover snap lag, and assumes the camera is the clipmap target.
`DevParams::ring_half_cells` carries `Clipmap::half_width` to the shader.

## 5. Material — `GridMaterial`

The terrain material is `ExtendedMaterial<StandardMaterial, GridMaterial>`;
one solid + one wireframe instance per clipmap, shared by all rings.

| Binding | Contents |
| ------- | -------- |
| 102 / 103 | heightmap / its sampler (103 is shared by overlay and stamp) |
| 108 | `texel_size` |
| 109 | `minmax` |
| 110 | `DevParams`: `SunShadowParams`, baked-light regions, debug view, stamp transform, `HoleBuffer` (48 × vec4), `hole_count`, ring half-width, `VtParams` (each level's visible rect, texel size, coverage, fade band) |
| 111 | `flags` (u32, below) |
| 114 | inline `HeightFogParams` |
| 115 / 116 | `editing`: edit overlay, stamp heightfield |
| 121 / 122 | clipmap albedo array / sampler (linear, repeat, no mips) |
| 123 | clipmap normal array |
| 125 / 126 / 127 / 130 | detail albedo / sampler / normal / ORM arrays |
| 129 | `DetailParams` |
| 132 | sun shadow-ceiling field (§7), sampled with 122 |
| 133 | `TerrainBakedLight` atlas, sampled with 122; its regions ride in `DevParams` |

`flags`: bit 0 wireframe · 1 unused · 2 single-layer detail ·
3 looping · 4 clay · 5 stamp preview · 6 stamp mask-weighted.

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
- `WILDERNESS_EDIT_OVERLAY` / `WILDERNESS_STAMP` — `editing` builds.

## 6. Shading cache — the texture clipmap

### 6.1 Levels and layout (`vt.rs`)

| Array | Format | Channels |
| ----- | ------ | -------- |
| albedo | `Rgba8UnormSrgb` | rgb albedo · a cavity |
| normal | `Rgba8Unorm` | rg octahedral world normal · b roughness · a packed material ids; second half: each layer's share, rgba |

The albedo array is `vt_size`² × levels and the normal array twice that, built once the heightmap loads (placeholders
until then, replaced in place so material bindings stay valid). Level 0's texel
is `vt_finest_texel`; each level outward doubles it while its window is smaller
than the world; the last level covers the whole world exactly and never moves.
At the defaults (1024², 0.5 m) an 8 km world gets 0.5, 1, 2, 4 and 8 m levels,
about 40 MB.

**Addressing is toroidal.** A level's texel for world `xz` is
`xz / coverage + 0.5`, wrapped by the Repeat sampler, so a texel never moves
in the layer as the window does; moving the window only rewrites the strips
entering it. Looping terrain needs nothing extra: the whole-world level's
coverage is the world.

**Windows** are centred on the clipmap's `target`, snapped to an eighth of the
window, so they move in strips of 128 texels at the default size.

### 6.2 Moving a level

Each frame `update_vt` compares each level's window with the one it has
composited (coarse levels first, so new ground gets a usable surface before a
sharp one). When it differs:

1. The texels about to be overwritten are the ones leaving the window, so the
   level's **visible rect** — what the main pass may read — narrows to the
   overlap of the old and new windows, in the same frame.
2. The strips entering the window are queued as requests (clipped to the world
   on finite terrain).
3. When the render world acknowledges every request, the visible rect widens
   to the new window.

**Requests reach the render world at extraction, never sooner**
(`VtOutbox` → `extract_vt_messages`). Under pipelined rendering the render
world draws the previous frame while this one runs, with that frame's wider
visible rects; a request it saw early would overwrite texels on screen. That
showed as flashes when backing away from terrain, where the leaving strips are
in front of the camera. Acknowledgements come back over a shared mutex
(`VtDone`): late only means a rect widens a frame later.

A rebuild (changed `vt_size` / `vt_finest_texel`) or a rebake cancels the
clipmap's queued requests. A rebake recomposites every window with the old
texels on screen until the new land.

### 6.3 The composite pass (`vt_render.rs`, `vt_composite.wesl`)

A system in the `RenderGraph` schedule's `Begin` set, so it lands before any
camera renders. It drains requests up to `vt_texels_per_frame`, splitting a
request by rows across frames, and splits each rect where it wraps the layer.
For each level touched it opens one render pass on that layer of both arrays
(MRT, so the splat runs once for both) and draws a fullscreen triangle per
piece, clipped by viewport and scissor. A fragment's world position is linear in
its framebuffer position (`CompositeDraw.origin + position · texel`), so the
layer samples' implicit derivatives are the level's texel size: **every level
is prefiltered at its own density**, and the levels are the mip chain.

All reads of the heightmap match the vertex shader, wrapping on looping
terrain.

- **Splat** (`splat_terrain`). Each layer's weight is the product of three
  optional bands — `SlopeRule` (degrees), `HeightRule` (meters) and
  `CavityRule` (terrain shape on the cavity scale: 0 hollow, 0.5 flat,
  1 ridge) — each a smoothstep ramp of width `blend` outside `[min, max]`.
  Cavity placement is what lets sediment collect in gullies and ridges stay
  bare, which slope and height can't tell apart. A layer with no rules
  is present everywhere (the "base layer" pattern, which keeps boundaries
  organic). Weights then go through **height blending**
  (`weight + albedo.a · height_blend`, keeping layers within 0.2 of the max),
  so grass fills the cracks between rocks instead of cross-fading. Every layer
  is sampled with **hex (stochastic) tiling** (Mikkelsen, 3 taps) to kill
  repetition — affordable because it runs once per texel, not per frame.
  Normals are reoriented onto the geometric normal, taken by central
  differences across the level's texel (or the heightmap's, if larger), so a
  coarse level's normal is its texel's average slope rather than an aliased
  point sample. The **top two layers** and their blend are packed into the
  normal target's alpha byte (`i0 + 4·i1 + 16·q`) for the detail overlay.
- **Cavity** (`cavity_at`, albedo alpha, and the splat's cavity bands): the
  depression angle of an 8-tap ring at r = max(2 texels, 16 m), ±15° mapped
  to 0..1 (0.5 flat) — resolution- and height-range-independent. Shading reads
  it only in debug view 1; it's there for material response.

About 50 texture reads per texel: the cavity ring, four heights for the normal,
and 3 hex taps × 3 textures × 4 layers.

### 6.4 The shadow field bake (`rvt.rs`, `bake.wesl`)

| Target | Format | Size | Channels |
| ------ | ------ | ---- | -------- |
| sun shadow | `Rgba16Float` | `sun_shadow_size`² | r shadow ceiling · g distance to occluder · b the column's own ground (r and b biased) |

**Shadow ceiling** (`shadow_ceiling`). For each column, the height below which
the terrain shadows it (and the occluder distance for penumbra width) — §7. A
march toward the sun out to 6 km whose steps grow by 2% (~210 samples): coarser
growth steps over ridges, so the ceiling comes out low and sun leaks into the
shadow near its edges.

`init_rvt` waits for the heightmap **and** a `DirectionalLight`; the bake's sun
is that light's `back()`, so the bake can never disagree with the scene's sun.
It spawns an inactive top-down orthographic camera (up = −Z, so texel layout
matches `xz / world_size + 0.5`) over a full-terrain quad on its own render
layer.

**Sentinel gating.** A camera that renders before its pipeline compiles and
textures are resident bakes black. So the target first draws to a 4×4 sentinel
that is read back every frame; the first non-black readback proves the pipeline
is ready, the real bake camera runs for 2 frames (`RVT_BAKE_FRAMES`), and
everything despawns. `TerrainSunShadow` is then marked valid; `ClipmapReady`
and `ever_baked` follow once the clipmap is composited too
(`mark_clipmap_ready`).

**Re-bake** (`editing`). Insert `RebakeRequested`; `process_rebake_requests`
re-runs the field's lifecycle, resizing it in place if `sun_shadow_size`
changed, and has the clipmap recomposite. Old shading stays on screen until the
new lands.

## 7. Shadows

There are **no realtime shadow passes in normal rendering** — terrain is
`NotShadowCaster`, and terrain shadowing is baked against a fixed sun:

- **On the terrain:** the same shadow-ceiling field as meshes (below)
  multiplies only the direct sun term, so shadowed valleys keep sky light and
  ground and players always agree. The ground tests the height the field
  recorded for its column (channel b), not its rendered height
  (`ground_sun_visibility_at`): distant rings sit below the true surface at
  peaks, under a lit column's ceiling (its own ground), and would read as
  shadowed. Both heights come from one fetch and filter, so a lit column sits
  exactly at its ceiling, lifted past the penumbra's midpoint into full sun.
  The field is coarser than the clipmap; its penumbra widens with occluder
  distance, which hides that at range. Raise `sun_shadow_size` if near ridges
  read too soft.
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

1. **Clipmap fetch** (`vt_sample`). Levels finest to coarsest: a level
   contributes where it's visible (fading over a band an eighth of its window
   wide at the rect's edge) and fine enough for the pixel's footprint (fading
   between one and two texels per footprint); what it leaves falls to the next,
   and the last level takes the rest. One level in the common case, two across
   a transition. The footprint is the minor axis of the world-space
   derivatives, widened toward the major by at most `VT_MAX_ANISO` (4), so
   grazing ground keeps some sharpness at the cost of slight aliasing along
   the view. Then the ground's sun visibility from the shadow field (§7).
2. **Near field**, faded by camera distance (`DetailConfig.near/far`) and
   skipped past `far` (derivatives hoisted out of the branch). Two paths:
   - **Two-layer detail (desktop): drawn from the materials.** Reads the layers'
     shares from the normal array's second half (B-spline filtered, so the
     texel grid doesn't print into boundaries), then samples every layer with
     a share at its `detail_tiling` — albedo with height in alpha, normal, ORM,
     biplanar on steep ground — and height-blends them per pixel. A layer's
     `micro_tiling` adds relief to its normal and its height to the blend
     within `micro_near/far`; the blend edge widens past it. The clipmap's
     colour over the plain mix of the same layers' means carries its macro
     variation (tints, broad tiling) onto the near colour, as brightness.
   - **Single-layer detail (standalone): grain over the clipmap.** Reads the
     packed material ids **NEAREST** with a world-locked ±½-texel jitter
     standing in for bilinear, and multiplies the top material's
     **unit-mean** brightness (`da / da_mean`) into the clipmap's albedo.
3. **Lighting** — `terrain_apply_lighting`, a trimmed fork of Bevy's
   `apply_pbr_lighting` (base layer only):
   - directional lights × CSM shadow (if any) × **field sun visibility**, with grazing
     specular suppressed on rough ground (dry terrain shouldn't look wet at
     sunset);
   - clustered point and spot lights with their own shadows (not the field);
     where baked light covers the ground, a light without
     `affects_lightmapped_mesh_diffuse` leaves its diffuse to the bake, as
     Bevy does for lightmapped meshes;
   - **baked light**, when a `TerrainBakedLight` covers the point: its
     irradiance re-aimed at the shading normal replaces the environment map's
     diffuse and the flat ambient, its sun visibility replaces the field's,
     and environment reflections are scaled by how much sky it holds;
   - environment map diffuse along the shading normal, with colored
     multi-bounce AO and specular occlusion from the detail AO, where there's
     no baked light. Then there's no sky occlusion: gullies take open-sky
     light. The baked cavity is shape,
     not occlusion (an open hollow is concave yet sees the sky), so it drives
     material placement rather than darkening;
   - flat ambient; then height fog, inline.
   This fork exists because Bevy has no hook for a per-pixel factor on the
   directional term only. **Re-check it on every Bevy upgrade** against
   `pbr_functions.wesl` (0.20 inserted rect area lights into the cluster
   ranges; terrain does not receive rect lights).
4. Deferred builds write `deferred_output` instead of lighting.

**Debug views** (`TerrainDebugView`, any build). Cavity and sun visibility
show a channel as grayscale, unlit. The rest measure what the shading costs on
a device, each removing one part: `Flat` (one colour, no texture reads or
lighting), `NoDetail`, `OneLevel` (the whole-world level only), `NoBakedLight`,
and `Unlit` (albedo and detail, no lighting or fog).

**Clay mode** (`editing`). While shading is stale (`clipmap.clay`, or never
ready) the fragment ignores the clipmap and renders grey clay with derivative
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
| `vt_size` | 1024 | texels per clipmap level side | live (rebuilds the clipmap) |
| `vt_finest_texel` | 0.5 m | level 0's texel; each level outward doubles it | live (rebuilds) |
| `vt_texels_per_frame` | 524 288 | composite budget per frame | live |
| `detail_layers` | 2 | 1 = skip the second material's detail samples | spawn or re-bake |
| `sun_shadow_size` | 2048 | shadow-ceiling field size | spawn or re-bake |

Bake-time fields are snapshotted when the clipmap spawns; changing them later
warns unless a re-bake is queued.

**Height fog.** One analytic exponential height-fog function
(`fog_functions.wesl`), per pixel in the terrain's shader with no pass of its
own, so flatscreen and VR match and MSAA stays free. Your own materials use it
through `InlineFog`. Set `HeightFog.max_distance` = camera `far`.

**Heightmaps.** R16Unorm KTX2 is the master format. 16-bit PNGs decode as
`R16Uint`; `retag_png_heightmaps` relabels them `R16Unorm` in place (identical
bytes) before anything reads them.

**Textures.** `load_terrain_array(images, paths, srgb)` decodes one file per
layer into a mip-chained 2D array (mips matter: coarse clipmap levels
sample them heavily minified). Normal maps are **OpenGL convention**; the bake and overlay flip X
to match the reorientation tangent — DirectX normals make cracks look raised.

**`Heightfield`** (`editing`): CPU bilinear height query matching the shader.
Requires a CPU-resident R16Unorm image; does not wrap for looping terrain.

## 10. Per-frame cost

| When | Cost |
| ---- | ---- |
| Every frame, CPU | ring snapping (writes only on snap), clipmap windows, change-gated quality/fog/holes sync |
| Every frame, GPU vertex | 4 heightmap loads (+ hole loop, + stamp in editing) |
| Every frame, GPU fragment, far | 2 clipmap samples (4 across a level transition) + 1 field sample + lighting |
| Every frame, GPU fragment, near | + 1 id load + 4 detail samples (8 when blending two materials) |
| While the target moves | a strip per level step, within `vt_texels_per_frame` |
| At load | the whole clipmap (5 levels × 1024² at the defaults) within the budget; the shadow field, ~2 frames |

**Measured** (2026-11-03, Intel Iris Xe, `basic` with `dev-controls`): a strip
of 131k texels (128 × 1024) composites in about 1.1 ms of GPU, roughly 8 ns per
texel. Flying at a normal speed stepped 1–3 strips a second and queued nothing.
The average is negligible; the cost to manage is the ~1 ms spike in the frame a
level steps, which a lower `vt_texels_per_frame` spreads across frames.

## 11. Invariants and gotchas

- **Terrain is origin-centered**; world size = heightmap texels × `texel_size`.
- **CPU ↔ GPU twins must stay identical:** `HoleBuffer::pack` ↔ the shader
  unpack; `DevParams` and `VtParams` ↔ the WGSL structs (field for field);
  `TerrainParams`, `CompositeTerrain`, `CompositeDraw` ↔ `vt_composite.wesl`;
  `Heightfield` ↔ `terrain_height`; the stamp preview ↔ the editor's commit;
  the clipmap's slot mapping (`k + size/2`, mod size) ↔ `xz / coverage + 0.5`.
- **A level's visible rect narrows before its texels are overwritten**, in the
  same frame the requests are queued, and requests travel by extraction (§6.2).
- **Avoid needless asset mutation.** `get_mut` on an image or material marks it
  changed (re-upload / bind-group rebuild); every sync checks before writing.
- **Material ids are read NEAREST** — filtering a packed byte bands.
- **Don't jitter splat bands with value noise** — thresholding lattice noise
  makes every material edge follow square cells. Use gradient noise if ever.
- **Hex cells must span ~1 texture repeat** — smaller collapses to speckle in
  the minified composite, larger shows repetition.
- **Shadow bake camera up is −Z**; `+Z` rotates the field 180°.
- Holes, the shadow field, the clipmap and fog are view-independent — safe for
  stereo, provided the clipmap's `target` is the head, not an eye.

## 12. Tried and rejected

Recorded so they aren't rebuilt without new information:

- **Camera-centred toroidal RVT ring** (one 1 km level, 0.25 m/texel,
  double-buffered, rebaked whole as the camera moved): worked, shelved for the
  VR frame budget, unmeasured. The texture clipmap (§6) is its successor: many
  levels, strips instead of whole rebakes, no double buffer, and a measured
  cost (§10).
- **Whole-world RVT** (one 8192² texture, baked at load): replaced by the
  clipmap — 1 m/texel near the camera, 512 MB, and a whole-terrain rebake per
  edit.
- **Page-table virtual texture** (sparse pages in an atlas, an indirection
  texture): planned, not built. The clipmap's residency is ring-shaped anyway,
  so it needs no page table, no new binding on a material at its limits, and
  one fewer dependent read per pixel.
- **Shipping the composited terrain with each level:** ~720 MB per level at
  8192² for data the client derives from the heightmap.
- **FFT horizon map** for terrain shadow: removed — redundant with a fixed sun.
- **Sky-occlusion channel** in a separate shadow texture: nearly invisible on
  open terrain; superseded by the ambient gather.
- **Ambient gather** (horizon macro AO + bent normal, its own RVT target) and
  the **ground's own sun march** (RVT albedo alpha): removed. The gather was
  the bake's heaviest pass and is to be replaced by baked lighting; the march
  disagreed with the shadow field that shades meshes, so ground and players
  could disagree about shade.
- **Painted control map** and **macro variation map**: removed when placement
  went fully procedural. Both are planned again (§13), as refinements of the
  rules rather than replacements: a level that paints nothing must still look
  right.
- **Erosion** (CPU droplets, then a GPU pipe model): removed from the editor —
  never produced convincing results. Landforms come from real-world import and
  external tools instead, with flow and wear masks computed from the heightmap
  (§13).

## 13. Open work

The game's ordering of this work, and why, lives in its own plan (Totem's
`docs/terrain-realism.md`). What's open in the crate:

**Stability and correctness**
- **Prefilter coarse rings.** Rings coarser than the heightmap sample it at
  their vertices only, so distant silhouettes swim as they snap. Give the
  heightmap a mip chain, kept in sync with edits, and sample the one matching
  each ring's cell size.
- **Specular anti-aliasing.** Roughness from normal variance (Toksvig) in the
  clipmap levels and detail normals, plus a geometric term from screen-space
  normal derivatives. Kills sparkle on rough ground, worst in VR.
- **Unbaked fallback outside `editing`.** Clay exists only with `editing`; a
  shipping build shows an unshaded surface until the clipmap is ready.
- **Edge coverage on finite terrain:** fade shading past the heightmap, where
  the geometry already drops.
- **Checks:** grid AABBs when `base_scale ≠ 1`; `Heightfield` wrapping on
  looping terrain.

**Texture clipmap**
- **Smooth level steps.** A level's visible edge jumps by an eighth of its
  window when it steps, so ground at that distance sharpens or softens at once.
  Have the edge retreat gradually. Waiting until it shows in a headset.
- **Dirty rects** from sculpting and painting, instead of recompositing every
  level on a rebake.
- **Cheaper coarse levels**, if Quest needs it: skip hex tiling and thin the
  cavity ring where their effect is below a texel.
- **Quest measurement** of the composite, with `vt_texels_per_frame` near 32k
  so a strip spreads over several frames.
- **Shadow field in the clipmap**, as another composited channel: sharp near
  shadows for ground and meshes alike. Parked: the march is the costliest
  per-texel work, and baked lighting already covers baked levels.

**VR**
- Clipmap `target` = the head; geomorphing measures its band per eye, so pass
  the head position too. Verify multiview; use eye-tracked foveation where the
  runtime offers it.

**Material layer** — most of what reads as real up close.
- Up to 16 layers (per-layer params leave their `Vec4` lanes); painted weight
  maps from the editor as a rule kind; height blending across every layer.
- Landform masks: flow routing across filled basins runs in straight streaks
  (needs flat resolution), and every edge is an outlet, so a looping seam is
  wrong. Masks drive `MaskRule`s and `MaskTint`s; flow drives neither yet.
- Curvature-driven response: a darker, smoother surface in hollows, weathered
  ridges.

**Beyond the surface**
- **Scatter:** GPU procedural placement from the same rules and masks
  (Horizon Zero Dawn's model), rocks before grass. Probably its own crate.
- **Object–terrain blending:** expose the clipmap so props sample it at their
  ground contact.
- **Close-up relief (desktop):** a denser level-0 grid displaced by the
  dominant material's height, plus POM.
- **CPU sun-visibility query** for gameplay.
- **Bigger worlds:** tiled heightmap streaming beyond ~8 km; the clipmap
  already pages the shading.
- **Time of day:** amortized re-bake of the shadow field as the sun moves.
- **Lighting fork:** replace `terrain_apply_lighting` with stock Bevy lighting
  if Bevy grows a per-pixel directional shadow factor.

**Editor**
- 8-bit stamps terrace on tall features: pre-blur on import.
- Tests for the ECS systems (cursor, dirty sync, re-bake debounce).
- A terrain-noise "generate" tool (ridged multifractal + domain warp) for quick
  base shapes.

**Stale comments**
- `world.rs` module doc and the UI world section describe only AWS tiles (USGS
  and Auto exist).
- `export.rs` and `sync_clay_shadows` mention Bevy 0.19.
- `ClipmapStamp` doc says "red channel (0..1)"; the editor supplies `R16Snorm` −1..1.
- `terrain.wesl` flags comment lists bits 0–2 only.
- `TerrainGesture::open()` has no callers since erosion was removed.
- The example header doesn't list Alt+wheel (stamp feather).

## 14. References

- [Terrain Rendering in Far Cry 5 (GDC 2018)](https://gdcvault.com/play/1025480/Terrain-Rendering-in-Far-Cry)
- [Adaptive Virtual Texture Rendering in Far Cry 4 (GDC 2015)](https://media.gdcvault.com/gdc2015/presentations/Chen_Ka_AdaptiveVirtualTexture.pdf)
- [GPU-Based Procedural Placement in Horizon Zero Dawn (GDC 2017)](https://www.guerrilla-games.com/read/gpu-based-procedural-placement-in-horizon-zero-dawn)
- [Procedural Grass in Ghost of Tsushima (GDC)](https://gdcvault.com/play/1027214/Advanced-Graphics-Summit-Procedural-Grass)
- [Runtime Virtual Texturing in Unreal Engine](https://dev.epicgames.com/documentation/unreal-engine/runtimevirtual-texturing-quick-start-in-unreal-engine)
- [Advanced VR Rendering — Valve (GDC 2015)](https://media.steampowered.com/apps/valve/2015/Alex_Vlachos_Advanced_VR_Rendering_GDC2015.pdf)
- [Stable Geometric Specular Antialiasing — Kaplanyan et al.](https://developer.download.nvidia.com/gameworks/events/GDC2016/akaplanyan_specular_aa.pdf)
- [Adreno texture best practices (Qualcomm)](https://developer.qualcomm.com/sites/default/files/docs/adreno-gpu/snapdragon-game-toolkit/gdg/gpu/best_practices_texture.html)
- [GPU Geometry Clipmaps (GPU Gems 2)](https://developer.nvidia.com/gpugems/gpugems2/part-i-geometric-complexity/chapter-2-terrain-rendering-using-gpu-based-geometry)
- [Hex-tiling (Mikkelsen)](https://github.com/mmikk/hextile-demo)
