# Terrain Editor Framework — Design Doc

Status: **Phases 0–11, 13 complete** — workspace + editing API; editor core;
sculpt; debounced re-bake; undo/history; feathered mask + overlay
visualization; background droplet + thermal erosion; looping seams + boundary
overlay; default egui UI + prop-placement demo tool; KTX2 + 16-bit PNG
export; new-terrain-by-default + runtime load (D9); rebake control + clay
display mode with dynamic terrain-scale shadows (D10); PNG stamp tool with
GPU floating preview + UI gallery (D11); erosion realism — slope-gated
droplets, flow-accumulation channels, smoothed deposition (D12).
**Phase 12 conditional** (progressive strip re-bake — measure the bake cost
first) · Project name:
**`bevy_wilderness`** · Last updated: 2026-07-18

> Note for later phases: the renderer's §3 anchors predate the workspace
> restructure — `src/…` paths are now `crates/bevy_wilderness/src/…`, and the
> former `lib.rs` grab-bag is split into `clipmap.rs` / `material.rs` /
> `quality.rs` / `dev_controls.rs`. The editor core lives in
> `crates/bevy_wilderness_editor`.
>
> Decisions made in flight: mask visualization is `Clipmap::edit_overlay`
> (`editing` feature) — an extra *texture* binding on `GridMaterial` sharing the
> heightmap sampler, gated by a shader def pushed in `specialize`, so it
> compiles out of non-editing builds byte-identically (the §10 bind-group
> ceiling is about *uniform* slots, which stayed untouched). Sculpt is
> mask-confined too, not just erosion (D4's intent generalized). Partial /
> region-scoped RVT re-bake was considered and **rejected for now** — the sun
> shadow is non-local (anti-sun corridor bounds are the classic seam-bug
> factory); if the full re-bake ever measures too slow, do *progressive strip*
> re-bake first (same total work, no hitch, zero correctness risk), regional
> only as a last resort.

## How to use this document

This is an **implementation brief for a future coding session** (likely Claude
Code). It records *what* we're building, *why*, the *decisions already settled*
(so they aren't relitigated), and the *order to build in* with acceptance checks.

If you're an agent picking this up:
1. Read §3 (existing renderer architecture) first — it anchors every relevant
   file/line so you don't re-derive the terrain pipeline.
2. Treat §4–§8 (principles, API, decisions) as settled unless the user reopens
   one. Each states its rationale — honor the *intent*, not just the letter.
3. Build in the phase order of §9. Each phase has an acceptance check.
4. §10 (gotchas) and §11 (open questions) are where this bites. Read them.

---

## 1. Vision

Build an **embeddable terrain editor framework** for Bevy, on top of
`bevy_wilderness` (this project — the renamed `bevy-clipmap` fork; see §7):

- **Sculpt** the heightmap in real time (raise / lower / smooth / flatten),
  geometry deforming live.
- **Mask** a region and run **hydraulic erosion** on it for realistic mountains —
  dendritic valleys, sharp ridgelines, sediment fans.
- Correct on **looping (toroidal)** terrain: edits + erosion wrap the seam.
- **Export** the result to a file so it ships as the terrain asset.

Crucially, it is a **framework, not an app**: a Bevy `Plugin` a host app adds,
exposing a **UI-agnostic API** and **extension points** so another app can embed
it and add its own tools. The open-source repo ships the editor + a **reference
egui UI** + a standalone example; a **closed-source game embeds the same plugin**
and registers its own tools (e.g. placing glTF props from its RON scene manifest)
against the API.

### Who consumes it

- **`bevy_wilderness` + editor** — open source (this workspace).
- **The game** — closed source. Embeds the editor plugin, reuses *or* replaces
  the reference UI, and adds a glTF-placement tool driven by its RON scene
  manifests. That tool lives in the game, built on the editor's API — the OSS
  editor never knows the game's manifest format.

## 2. Non-goals

- **Not used in VR.** The editor is a **desktop authoring tool**; its *output* (a
  baked heightmap) is what respects the VR budget. This frees the editor to spend
  desktop RAM/threads. The runtime stays static per [[no-realtime-shadows-vr]],
  but that constraint does not bind the editor itself.
- **Not real-time-per-frame erosion.** A few seconds per erosion run is fine —
  the user chose quality over speed.
- **No game-specific assets/logic in the OSS crates.** RON manifests, the game's
  glTF placement, its UI — all live in the closed-source game, built on the API.
- **No new runtime cost in `bevy_wilderness`'s render path.** The only thing
  the renderer gains is a small, opt-in **editable-terrain API** (§5). All
  editor systems live in separate crates.

---

## 3. Existing renderer architecture (anchors)

Facts the editor builds on, with locations. These describe the current renderer
code (the `bevy-clipmap` fork being renamed to `bevy_wilderness`, §7); the
`src/…` paths stay valid post-rename. Verify against current code before relying
on a line number.

- **Geometry is displaced live in the vertex shader.** The clipmap is a flat
  grid; `fn vertex` (`src/terrain.wgsl:119`) samples the heightmap (binding 102,
  `src/terrain.wgsl:34`) every frame. **Mutating heightmap texels deforms the
  terrain next frame — no mesh rebuild.** This is why live editing is free.
- **Heightmap is single-channel 16-bit** (`R16Unorm`), 1024² in the example, KTX2
  with `is_srgb=false` (`examples/basic.rs:233-238`).
- **A CPU heightfield reader exists**: `Heightfield` (`src/lib.rs:675`, currently
  private) — world↔texel math, bilinear `height()`, `contains()`. Requires
  `R16Unorm` + a CPU-resident image (`MAIN_WORLD` usage). `SunVisibility`
  (`src/lib.rs:640`, public) wraps it.
- **Shading is baked once, not shaded per-frame.** The RVT bake (`src/rvt.rs`,
  `init_rvt` at :279) renders material splat + sun self-shadow + AO/bent-normals
  to textures via top-down ortho cameras → `RenderTarget::Image`; the main pass
  samples those. **Until a bake completes the terrain is a chrome mirror.**
- **The bake is one-shot, self-despawning.** `drive_rvt_bake` (`src/rvt.rs:134`)
  despawns each bake camera + quad when done (`src/rvt.rs:160`). ⚠️ **Must become
  re-triggerable** (§5).
- **Material placement is slope/height based**, baked via `TerrainParams`
  (`src/rvt.rs:27`) from `SlopeRule`/`HeightRule` (`src/lib.rs:80,91`). Erosion
  changes slope hard, so a re-bake is what makes carved cliffs *turn rocky*.
- **Looping is already toroidal in the shaders.** Reads wrap: bake at
  `src/bake.wgsl:71` (`idx = ((p % size) + size) % size`), vertex via flags bit3,
  RVT targets use a Repeat sampler (`looping_rvt_sampler`, `src/texture.rs:18`).
  **A looping heightmap has no seam** — position 0 and position *width* are the
  same terrain.
- **RVT resolution** is `TerrainQuality::rvt_size` (`src/lib.rs:898`), default
  8192², independent of heightmap resolution.
- ⚠️ **`GridMaterial` is at the bind-group ceiling** (`src/lib.rs:509` banner):
  do **not** add texture/uniform bindings — extra uniforms silently break the
  pipeline. Pack into the existing `flags` u32.
- **`Clipmap` fields are `pub`** (`src/lib.rs:186`, incl. `heightmap`), so an
  external crate can own the heightmap image and swap it in.

---

## 4. Core design principles

Three principles govern the whole build.

### P1 — f32 authoritative height field; derived R16 display map
Sculpt + erosion mutate an **`f32` height field**. The **`R16Unorm` display
heightmap** the vertex shader reads is derived from it (cheap quantize of the
dirty region). **Why:** R16 over ±1312 m gives one step ≈ 0.04 m; erosion moves
*sub-LSB* sediment per iteration, which rounds to zero in R16 and stalls/bands.
The f32 field also unifies sculpt and erosion — both are edits to one buffer.

### P2 — UI-agnostic editor core + optional default UI
The editor **core** owns state and behavior and exposes it as **resources +
events + components**. It **never assumes egui**. A separate **default UI** crate
drives the core through that API. **Why:** the closed-source game wants terrain
tools inside *its own* editor UI, next to its glTF-placement tools. A hard-coded
UI can't merge; a UI-agnostic core lets the game reuse the default UI *or* build
its own against the same API without forking. The default UI is also how we
**dogfood** the API — if it can't do something cleanly, the API is wrong.

### P3 — The editor is a Plugin with extension points
The editor is a Bevy `Plugin` a host app adds. It exposes **extension points** so
a third-party tool (the game's glTF placement) coexists with the built-in tools:
a **tool/mode registry**, a **shared terrain raycast**, **edit events**, and a
**terrain-height query**. See §6. **Why:** "extend another bevy app" = embeddable
plugin, not standalone binary. The OSS standalone example is *itself* just a host
app adding the plugin — so it exercises the exact embedding path the game uses.

---

## 5. API `bevy_wilderness` must expose (consumed across the crate boundary)

Editing the heightmap from an external crate already works (own the image, write
texels, geometry follows). **Re-baking does not** — the bake pipeline is entirely
`pub(crate)` and one-shot. So `bevy_wilderness` grows a small, opt-in **editable-
terrain API** (behind an `editing` feature, so default consumers gain nothing):

1. **Re-bake trigger** *(the big one; nothing public today).* A public
   `RebakeRequested` component/event that re-runs the sentinel-gated bake and
   re-tallies `pending_bakes` → `ClipmapReady`. Requires refactoring the one-shot
   self-despawning bake (`src/rvt.rs:160`) to re-spawn (or retain + re-activate)
   its cameras/quads.
2. **CPU-resident heightmap as a supported mode.** The editor owns/creates the
   heightmap `MAIN_WORLD | RENDER_WORLD` and assigns the handle to `Clipmap`
   (field already `pub`). Document this as supported; the example currently loads
   render-only — one-line change.
3. **Expose `Heightfield`** (`src/lib.rs:675`) so the editor reuses the
   world↔texel + bilinear math instead of reimplementing ~90 lines that must stay
   in sync with the shaders.

---

## 6. Editor-core API surface (what host tools bind to)

The editor core exposes, UI-agnostically:

- **Tool registry** — one active tool at a time (built-ins: Sculpt, Mask, Erode).
  A host app registers its own tool (the game's Place-glTF). Shared camera,
  selection, and input focus.
- **Shared terrain raycast / cursor-hit** — cursor → terrain world point, via
  `Heightfield`. Every tool uses the *same* pick so a placed prop lands exactly
  where the brush would paint.
- **Terrain-height query** — height at (x,z) (`Heightfield` / `SunVisibility`), so
  host tools snap props to the surface.
- **Edit events** — emitted on sculpt/erosion/re-bake: `TerrainRegionChanged { aabb }`.
  Host tools subscribe to **re-snap** placed props when terrain under them
  changes. ⚠️ The hook people forget and then fight later — include it from day 1.
- **Brush / mask / erosion state** — resources the UI reads/writes (mode, radius,
  strength, mask handle, erosion params).

The OSS demo includes a **generic prop-placement tool** (place a plain glTF/cube,
snap to terrain, re-snap on `TerrainRegionChanged`) to prove a third-party tool
plugs in without the editor knowing any manifest format.

---

## 7. Project name, crate & repo layout

### Project name — `bevy_wilderness` *(settled 2026-07-14)*

The heavily-diverged `bevy-clipmap` fork is renamed to **`bevy_wilderness`**. Why:
it can't be published under the taken `bevy-clipmap` name; the fork has diverged
too far to upstream; and "clipmap" names an *implementation* (the LOD technique)
the renderer might one day swap, whereas the editor is engine-agnostic — so the
family name should be domain-level, not mechanism-level. "Wilderness" reads as
*beautiful, walkable, nature-shaped terrain* (nature-shaped = the erosion feature),
is planet-neutral ("the Martian wilderness"), and claims no category term (unlike
`landscape`, which is the industry term for the whole terrain-system category).

- **Do the rename at workspace-creation time (Phase 0).** It touches every crate
  name, import path, and `repository` field anyway, and there are no external
  dependents yet — near-free now, only costlier once published.
- **Rename in place — preserve git history**, don't start a fresh repo (keeps the
  fork lineage that backs MIT attribution).
- **MIT attribution:** keep kirillsurkov's copyright + MIT text on the inherited
  files, add your own copyright for new code, and credit the origin in the README
  ("forked from bevy-clipmap by kirillsurkov").
- **Claim the crates.io name only at first publish** (currently unclaimed, as are
  `bevy_wilderness_editor` / `_editor_ui`).

### Workspace layout

Single **open-source Cargo workspace**. Rationale for workspace over separate
repos: the crates **co-evolve** (the editor keeps needing new `bevy_wilderness`
API), so editing both in one commit with no publish cycle matters; a shared
`Cargo.lock` **enforces one bevy version** across all crates (separate repos drift
and fight on every bevy upgrade); single build/test/CI. Publishing is per-crate
(`cargo publish -p …`, `publish = false` on non-published members), so a workspace
does **not** force publishing everything. The whole workspace is public; the game
stays a separate closed-source consumer.

```
bevy_wilderness            (OSS lib)  terrain render + opt-in editable-terrain API
                                      (§5). The renamed bevy-clipmap fork.
bevy_wilderness_editor     (OSS lib)  sculpt/mask/erosion/overlay core + UI-agnostic
                                      API + extension points (§6). Deps bevy_wilderness.
bevy_wilderness_editor_ui  (OSS lib)  default egui UI driving the editor API (P2).
                                      Optional — game may replace it. Hosts
                                      examples/editor.rs: the minimal host app
                                      (plugins + default UI + demo prop-placement
                                      tool) exercising the embed path.
─────────────────────────────────────────────────────────────────────────────────
your game                  (closed)   adds editor plugin (+ own or default UI),
                                      registers RON→glTF placement as a tool on §6 API.
```

`_editor_ui` as its own crate (not a feature of `_editor`) is the cleaner cut,
since the game will want the editor *without* the default UI while others want it
*with*. It can start as a feature and graduate to a crate if that gets awkward.

---

## 8. Feature decisions (settled)

### D1 — Working resolution: 4096²
User requirement. f32 field 67 MB, R16 display 33 MB, + erosion working buffers
(~67 MB each). ~300 MB CPU-side during a run — fine on desktop. Load the heightmap
`MAIN_WORLD | RENDER_WORLD`.

### D2 — Sculpt brushes, live, toroidal-aware
Raise / lower / smooth / flatten; adjustable radius + strength; radial falloff.
**Held modifiers override the mode** — Shift smooths, Ctrl inverts (a sign flip
on the per-texel delta, so raise↔lower, smooth→sharpen, flatten→exaggerate) —
so the two moves a stroke constantly reaches for don't cost a trip to the mode
row. Cursor→terrain via the shared raycast. Edit f32 field → re-quantize dirty region
to R16 → geometry follows next frame. When `looping`, brush footprints **wrap
modulo heightmap dimensions**.

### D3 — Erosion: droplet-based hydraulic (CPU, background) + thermal
**Droplet simulation**, not the grid pipe-model — with real-time pressure off,
droplets give the realistic result (dendritic drainage, V-valleys, ridgelines,
sediment fans). ~1–3 M droplets for 4K density, ~30–80 steps each. Run on
`AsyncComputeTaskPool` (no freeze); apply to the f32 field on completion, then
re-quantize + re-bake. **Multithread via per-batch delta buffers summed** —
droplet writes scatter, can't share one buffer. Add a **thermal** pass
(slope-limited talus to the angle of repose). **Masking bounds cost** — the common
case erodes a masked range, not the full 4K map.
- *Upgrade path only if full-map 4K is too slow:* GPU **compute** droplet sim with
  atomic sediment accumulation (Sebastian Lague approach) → sub-second. ⚠️ Repo
  has **no compute plumbing** (it fakes compute with fragment bakes; droplet
  scatter can't be a fragment pass) — genuinely new machinery. CPU first.

### D4 — Masking with feathered edges
Paint a mask (reuse brush infra) to confine erosion. **Feather the edge**: weight
erosion deltas by the mask so eroded ground blends into untouched ground — no hard
rectangular seam.

### D5 — Debounced full re-bake on release
Geometry updates live; the baked shadow/AO/material-splat re-runs on a ~200 ms
debounce after a stroke/erosion settles (via the §5 re-bake API). Matters *more*
for erosion: it changes slope, and the splat places rock by slope, so the re-bake
turns fresh cliffs rocky. Fires `TerrainRegionChanged` for host tools.

### D6 — Seam painting seamless by construction
On looping terrain there's nothing to stitch — make every edit toroidal (D2) and
erosion use wrapped neighbor reads (droplets exiting one edge re-enter the
opposite); the bake already wraps (`src/bake.wgsl:71`). Add a **visibility aid**
(wrap line is invisible by design): a toggleable **gizmo overlay** of the tile
boundary and/or **ghosted neighbor copies** so edits near an edge show on the far
side live.

### D7 — Export to file
Export the R16 heightmap to a file that round-trips with the existing loader
(`examples/basic.rs:233`). Format *(settled in Phase 8)*: **both**, chosen by
the requested path's extension —
- **R16 KTX2** (engine master): the only format that round-trips — ⚠️ bevy
  0.19 decodes 16-bit grayscale PNG to `R16Uint`, *not* `R16Unorm` (found in
  acceptance testing: `Uint` sampler mismatch, editor rejects the load), so
  PNG cannot be the renderer/editor master. KTX2 carries an explicit
  `vkFormat` the loader trusts. Written by hand (~40 lines: header, index,
  DFD, data — the `ktx2` parser crate has no writer).
- **16-bit grayscale PNG** (interchange): required by the game — its physics
  pipeline builds the Avian collision heightfield from the PNG via standard
  image decoding (which is lossless for R16, just off the engine's loader
  path). Also opens in DCC tools. The *renderer* loads it too:
  `R16Uint` is byte-identical to `R16Unorm`, so `ClipmapPlugin` retags a
  clipmap's Uint heightmap in `PreUpdate` (a relabel, not a transcode),
  before `init_clipmaps` or render extraction read it. So a game can ship
  *one* PNG for both terrain and physics: same VRAM/runtime cost as KTX2,
  smaller on disk (zlib), at a one-time decode cost on load (~100 ms at
  4096²). KTX2 remains the memcpy-fast option.

Round-trip requires the same `min`/`max` encode range on the loading
`Clipmap` (inherent to R16, same as the shipped asset).

### D8 — Undo/history: tile-based region snapshots *(settled 2026-07-16)*
Not full-field copies (67 MB each at 4096²). The field divides into fixed tiles
(64² texels ≈ 16 KB); an input gesture (stroke press→release, one erosion run)
opens an **undo entry** that copies each touched tile's *pre-edit* data on first
touch, then seals on release. **Undo** writes the saved tiles back and
`mark_dirty`s them — the existing sync path then handles re-quantize,
`TerrainRegionChanged` (prop re-snap), and the D5 re-bake debounce, so undo gets
correct shading for free. **Redo** saves the current tiles into the entry before
restoring. History is a ring buffer capped by **total bytes** (~256 MB), evicting
oldest — one fat erosion entry and fifty thin brush dabs cost what they touch.
Two early commitments:
- **Entries span buffers**: an entry is a set of *(buffer, tile, old data)*, not
  height-specific — the Phase 4 mask (and any later layer) is undoable with the
  same machinery.
- **Core API, not UI** (P2): an `UndoHistory` resource with undo/redo methods;
  the UI (or a Ctrl+Z keybind in the example) merely calls them.

### D9 — New terrain is the default state; loading is opt-in *(settled 2026-07-17)*

A **new terrain** — a flat plain at the D1 4096² working resolution — is the
editor's **default starting state**, not a special mode. Authoring usually
*begins* from scratch, so that's what the editor should open on; loading an
existing heightmap is the exception you ask for. This reframes the earlier
`WILDERNESS_NEW`-gated "from scratch" path: the example now opens a new terrain
with no env var, `WILDERNESS_NEW=<texels>` merely picks its starting
resolution, and `WILDERNESS_HEIGHTMAP=<file>` is the opt-in load (still the D7
round-trip).

Runtime new/load are core API, UI-agnostic like the rest (P2): messages
`NewTerrainRequested { terrain, size, height }`, `LoadRequested { terrain,
path }` → `TerrainLoaded { terrain, path, error }`. Both swap the field and
display heightmap on a live `Clipmap`, **keeping the world footprint and R16
encode range** (a different-resolution map just re-derives `texel_size`), then
mark the whole field dirty so the normal Apply path re-quantizes, re-snaps
props (`TerrainRegionChanged` over the full map), rebuilds the mask overlay at
the new resolution, and re-bakes; the undo history clears (its tile snapshots
belong to the replaced field). Load reads the file **synchronously off the
filesystem** — not the asset server — so a host's file dialog can hand it an
*absolute* path (asset paths are rooted at the assets dir), and a failed decode
leaves the current terrain untouched. The default UI adds a "New" button (with
a resolution combo) and a "Load terrain…" button backed by a native file
dialog (`rfd`, xdg-portal backend — no GTK dependency).

### D10 — Rebake control + clay display mode *(settled 2026-07-17, planned: Phases 9–10)*

Real-time modeling needs the bake out of the interaction loop. Today the D5
debounce fires a **full 8192² re-bake ~200 ms after every pause**, mid-session
— a potential hitch right in the middle of an edit — and until it lands, old
shading smears over new geometry. Two pieces:

1. **Rebake control.** A `RebakeSettings { auto: bool }` resource (default
   `true` = current D5 behavior). When off, dirty flushes don't arm the
   debounce. Manual trigger is the *already-public* `RebakeRequested` — no new
   renderer API; the default UI gets an auto toggle + "Bake" button. Workflow:
   batch a modeling session, bake once.
2. **Clay display mode.** While shading is stale (debounce pending, auto off
   with unbaked edits, or the initial bake still in flight) the terrain
   renders as **neutral grey clay**: a `flags` bit (packed per §10 — no new
   bindings) switches the fragment shader to screen-space-derivative normals
   + simple N·L, ignoring the stale RVT. **Why grey, not stale shading:**
   "geometry is truthful, materials are pending" is a coherent statement;
   old rock splat stretched over a new mountain is not (ZBrush's clay is the
   model). **Whole-terrain, not stale-region-only:** modeling mode is a
   mental state, not a region; region-clay needs a stale-mask texture
   (binding friction) and mixed clay/shaded terrain reads as broken.
   Side effect: clay supersedes the §10 "unbaked terrain is a chrome mirror"
   gotcha — it's the principled fallback for *any* unbaked state, including
   first load.
3. **Dynamic shadows while clay** *(added in flight)*. Clay renders with
   sun-visibility 1.0 (no valid bake to shadow with), so while any terrain
   is clay the renderer turns on the directional lights' CSM, makes the
   solid terrain parts cast, and swaps in **terrain-scale cascades**
   (`maximum_distance` = the terrain's world footprint; bevy's ~1 km default
   is character-scale and editing happens from altitude). Same 4 cascade
   renders either way — extending range costs texel density, not framerate.
   Everything is saved and *restored* on exit, so a host's own light config
   survives. Two supporting pieces:
   - `GridMaterial` gained a **prepass vertex shader** (the shadow pass
     renders casters with it; without it terrain would cast its flat,
     undisplaced grid), and `terrain.wgsl`'s fragment machinery is gated
     behind a `WILDERNESS_SHADE` def pushed in `specialize` — depth-only
     pipelines compile the displacement vertex alone.
   - ⚠️ **Known bevy-internals coupling (re-verify on every bevy
     upgrade):** bevy 0.19 renders *opaque* shadow casters through a
     depth-only path whose pipeline layout omits the material bind group —
     the displacing vertex shader can't read the heightmap there. The only
     route into the material-bound shadow path is `MAY_DISCARD`, so while
     clay is forced the solid material's base `alpha_mode` flips to
     `Mask(0.0)` (nothing discards at cutoff 0; visually identical),
     restored to `Opaque` on exit. The unused `PREPASS_READS_MATERIAL` key
     bit looks like the future official API — switch to it when bevy
     exposes one. Entering/leaving clay re-specializes pipelines (small
     one-time hitch per session).

### D11 — PNG stamp tool: GPU floating preview + UI stamp library *(settled 2026-07-17, planned: Phase 11)*

Stamp a grayscale heightfield PNG (**both 8- and 16-bit**; the 16-bit load
path exists via the R16Uint retag) into the terrain as a displacement
(add/subtract, adjustable strength/scale/rotation), with a **live preview
floating under the cursor** before commit.

- **Preview is GPU-side, by necessity.** Stamps may cover **half the map**, so
  the CPU "floating edit" (save footprint → apply → restore next frame) is
  out: ~8 M texels of restore + re-quantize + re-upload per moved frame. The
  vertex shader composites instead: sample heightmap, then if the stamp flag
  bit is set, project world XZ through the stamp transform and add
  `strength × sample`. Zero per-frame CPU at any stamp size.
- **Bindings fit under the §10 ceiling.** The stamp texture is an
  editing-gated binding behind a shader def — the exact `edit_overlay`
  precedent. The transform params (position/scale/rotation/strength/mode)
  **grow an existing uniform struct** rather than adding one: ⚠️ the ceiling
  is *binding slots*, not uniform sizes.
- **Mask-weighted, preview and commit alike — for free.** Commit weights
  deltas by `mask_weight` (erosion's pattern, feathered edge included). The
  preview honors the mask by sampling `edit_overlay` — the mask's
  visualization texture, *already bound in the material* — so preview matches
  commit by construction, no new binding.
- **Commit = one CPU apply** into the f32 field, same math as the shader
  (bilinear sample + weighted add — keep them trivially identical so nothing
  pops the frame the preview flag drops). One D8 undo gesture (a half-map
  snapshot ≈ 33 MB fits the 256 MB ring). Toroidal: stamp UV math wraps, so a
  seam-crossing stamp previews and commits on the far side (D6).
- During preview, `TerrainHeight`/raycast see the *base* terrain (the field is
  untouched until commit) — fine: the user is aiming, not standing on it.
- **Controls:** wheel = strength, Ctrl+wheel = scale, Shift+wheel = rotate.
  ⚠️ The free camera uses plain wheel for fly speed — while the stamp tool's
  preview is floating, the wheel belongs to the stamp (tool-scoped input
  priority, same spirit as `PointerBlocked`).
- **Stamp library is UI-only (P2).** The core holds just the active stamp
  (image + `StampSettings`) — a host can feed stamps from anywhere. The
  default UI owns a configurable stamps folder (the `UiExportPath` pattern),
  scans it for PNGs, shows the current stamp as a clickable thumbnail, and
  clicking opens a gallery grid. Ship a few CC0 example stamps so the gallery
  isn't empty on first run.

### D12 — Erosion realism: slope-gated droplets, flow-accumulation coupling, smoothed deposition *(settled 2026-07-18, planned: Phase 13)*

The D3 droplet sim works but reads noisy — pockmarks everywhere. Three
verified causes: the `min_sediment_capacity` floor lets freshly spawned
droplets (sediment = 0 < capacity) carve flat ground at their uniformly-random
spawn points (~1.7 M random pits at 4096²); carving spreads over the radius-3
brush but deposits land as single bilinear points (1-texel bumps); and the
flat-ground random-wander branch carves random-walk scratches at min-capacity.
Goal: maximum realism, quality-over-speed (runs are already background
tasks with a progress bar). Flat areas get *realistic* treatment — texture
arrives by deposition, not in-situ pitting; there is deliberately no
"pristine flats" mode (the D4 mask already protects authored areas).

- **Slope-proportional capacity replaces the floor.** `capacity = slope ×
  speed × water × sediment_capacity` — flat ground gives ≈ 0 capacity, so a
  laden droplet deposits and an unladen one does nothing; that *is* the
  deposition-floor behavior, so `min_sediment_capacity` is **removed**
  (⚠️ breaking `ErosionSettings` change; no workspace usage outside erosion
  code). A smooth low-slope gate (`min_slope_deg`, default 0.5°, converted
  per-job like `talus`) fades `erode_rate` in over the last fraction of a
  degree so micro-noise slopes aren't nibbled at full rate.
- **Warm-up + stagnation death.** Droplets may not carve their first 2 steps
  (spawn-point shot noise gone); a droplet with no momentum *and* no gradient
  deposits its load and dies instead of wandering randomly — flat-spawned
  droplets become nearly free, which keeps uniform rain affordable.
- **Brushed deposition.** Deposits spread over a falloff brush
  (`deposit_radius`, default 2) at all three sites — mid-flight, pit-fill,
  and death — mirroring the erosion brush; no more 1-texel bumps.
- **Per-round D8 flow accumulation, coupled as a capacity multiplier only.**
  Every texel gets one unit of rain; cells drain to their steepest-descent D8
  neighbor, processed high-to-low (height sort **tie-broken on index** —
  `sort_unstable` on plateau ties is nondeterministic otherwise), wrap-aware,
  log-normalized (`ln(1+A)/ln(1+A_max)` — a power norm would zero the
  tributaries under the trunk-stream max). Capacity gains `× (1 +
  flow_strength × w)`: rain falls everywhere, erosive power concentrates in
  channels — the dendritic look. Spawn stays **uniform** (weighting spawn
  would starve hillslopes of the diffusive traffic that produces slope
  texture and flat-area deposition). A separate stream-power grid pass
  (`E = K·A^m·S^n`) was **rejected**: the boosted droplet *is* a stochastic
  stream-power integrator, and grid incision detaches material with no
  sediment routing — it would carve without feeding the fans. Escalation
  path if channels still read shallow at max `flow_strength`.
- **Split erode/deposit accumulators + deposit-only blur.** Batches return
  separate erode/deposit buffers; each round the deposit accumulator gets a
  separable wrap-aware box blur (`deposit_blur_radius`, default 2) **before**
  mask weighting — fans and valley fill read smooth while channel walls stay
  crisp, and the mask still hard-confines (zero-weight texels drop blurred
  deposits; same class of mass loss as a droplet exiting the mask today).
- **Analysis maps.** `keep_maps` (default off) retains per-run wear/deposit/
  flow maps as an `ErosionMaps` component — host API for future splat/
  auto-texture use, no UI.
- **Cost honesty.** Flow adds a sort-dominated ~15–25 s over 8 rounds at
  4096² (roughly doubling a run); split buffers roughly double transient
  memory (~1.0–1.1 GB peak at 4096² masked). Both fine for a background run
  with a progress bar; escape hatches (flow every other round,
  `MAX_BATCHES` 4→3) noted, not built.
- Defaults shift for quality: `inertia` 0.05→0.15 (longer, smoother
  channels; carries direction across flats), `droplet_density` 0.1→0.15,
  `max_lifetime` 64→96 (droplets reach valley floors at 4096²). UI: density
  slider range fixed to 0.01..=2.0, new collapsed "Realism" section
  (min slope, inertia, flow carving, deposit blur).

---

## 9. Build phases (in order; each with an acceptance check)

**Phase 0 ✅ — Workspace + rename + editable-terrain API.** Stand up the workspace
(§7) and rename the fork to `bevy_wilderness` (in place, preserve git history);
add its `editing` feature: re-bake trigger (refactor §5.1), CPU heightmap mode,
expose `Heightfield`. *Accept:* an external crate triggers a re-bake on a running
clipmap and shading updates.

**Phase 1 ✅ — Editor core skeleton.** Editor plugin; tool registry, shared raycast,
`TerrainRegionChanged` event, height query, brush/mask/erosion state resources
(§6). f32 authoritative field + R16 quantize (P1). *Accept:* terrain renders from
the derived R16 map; a no-op registered tool receives raycast hits + edit events.

**Phase 2 ✅ — Sculpt tool.** Raise/lower/smooth/flatten editing the f32 field (D2).
*Accept:* dragging deforms terrain live; radius/strength adjustable.

**Phase 3 ✅ — Re-bake on release.** Debounced re-bake + `TerrainRegionChanged`
(D5). *Accept:* sculpt a hill, release, shadows/AO/rock-placement update to match.

**Phase 3.5 ✅ — Undo/history.** Tile-based region snapshots + byte-capped ring
buffer (D8). Placed before erosion deliberately: a 5-second erosion run you
don't like is exactly what undo exists for. *Accept:* sculpt, undo (Ctrl+Z in
the example) — terrain, shading, and prop re-snap all revert; redo restores;
history survives deep strokes without unbounded memory.

**Phase 4 ✅ — Mask tool.** Feathered mask paint (D4), undoable via D8's
multi-buffer entries. *Accept:* a mask confines a subsequent op with a soft,
seamless edge.

**Phase 5 ✅ — Erosion.** Masked droplet + thermal on `AsyncComputeTaskPool` (D3).
*Accept:* mask a lump, erode, get dendritic valleys/ridgelines in a few seconds
without freezing; re-bake shows rock on new cliffs.
Decisions in flight: the sim works in *normalized* height units (0..1 of the
encode range) so the standard droplet parameters stay scale-independent; a run
is 8 sequential *rounds* of parallel per-batch delta buffers (≤ 4, each a
full-field f32 — memory cap), applied mask-weighted between rounds so later
droplets follow earlier rounds' channels (the dendritic feedback); thermal is
steepest-neighbor talus shed, mask-weighted at the source, region-bounded to
the mask + one texel of creep per iteration; results land as *deltas* (not
absolute heights) so edits made during the run survive; the apply defers while
`UndoHistory::gesture_open()` so it never splits another tool's stroke entry.
API: `ErosionRequested` message (the erode tool writes it on click; a host UI
writes it directly) + `ErosionRun` component with `progress()`. Droplet budget
is `ErosionSettings::droplet_density` (droplets *per texel* of eroded area,
default 0.1) — an absolute count over-eroded small maps/masks by their area
ratio, so the knob is a density; spawn positions are rejection-thinned by the
feathered mask weight.

**Phase 6 ✅ — Looping seams.** Toroidal edits + wrapped erosion + boundary overlay
(D6). *Accept:* with `looping` on, a stroke/erosion across an edge is continuous
and the tile still repeats seamlessly.
Decisions in flight: toroidal edits and wrapped erosion were pre-paid in
Phases 2/4/5 (`wrap_texel`/`wrap_rect`, droplet re-entry, wrapped thermal
neighbors — all tested), so this phase delivered the D6 visibility aid plus
validation. D6's "ghosted neighbor copies" idea is moot — a looping clipmap
*renders* the repeats, so edits near an edge already show on the far side
live; the aid that was actually missing is the invisible-by-design wrap line.
`SeamOverlay` resource (default on) draws a gizmo ring hugging the surface
around the tile instance under the camera — neighboring instances share
edges, so one ring marks every nearby seam and follows the camera from repeat
to repeat. Also validated: sculpting while standing on a *repeat* lands on
identical base-tile texels (the cursor's texel coords are a whole tile offset;
`wrap_texel` resolves them).

**Phase 7 ✅ — Default UI + demo tool.** `bevy_wilderness_editor_ui` egui panels;
`examples/editor.rs` host app + generic prop-placement tool (§6). *Accept:* the
example drives every tool via the UI; the prop tool places + re-snaps a glTF on
`TerrainRegionChanged`, proving the extension API.
Decisions in flight: `bevy_egui` 0.41 (egui 0.35 — panels attach to a root
`Ui`, not the `Context`; UI systems run in `EguiPrimaryContextPass`). The one
core-API gap dogfooding exposed: input focus — added `PointerBlocked`, a
resource any UI writes before `EditorSet::Pick` so the shared pick (and thus
every tool) goes quiet while the pointer is over panels; that's the whole
UI↔core input handshake, and egui types stay out of the core (P2). The panel's
tool list renders from the `EditorTools` registry, so host-registered tools
appear with zero UI changes. The example moved to
`crates/bevy_wilderness_editor_ui/examples/editor.rs` (it needs all three
crates; dev-dep direction stays acyclic). Demo prop tool places cubes (plain
mesh, no asset dependency) via the shared pick and re-snaps them from
`TerrainRegionChanged` + `TerrainHeight` — the game's glTF tool is this shape
plus a manifest. Erosion sliders exposed: droplet density, capacity,
erode/deposit rate, talus angle (§11's "a few sliders").

**Phase 8 ✅ — Export.** Write the R16 heightmap to file (D7). *Accept:* export,
restart loading the exported file, terrain matches.
Decisions in flight: dual format by extension — KTX2 engine master + PNG
interchange for the game's Avian collision build (see D7 for the full story,
including why PNG-only failed in acceptance). Core API is the message pair
`ExportRequested { terrain, path }` → `HeightmapExported { terrain, path,
error }`; one request per file, exports run concurrently on
`AsyncComputeTaskPool` (a 4096² map is ~33 MB). The UI's Export button writes
*both* formats beside the host-configurable `UiExportPath` base; the example
points it into the renderer's assets dir and takes a `WILDERNESS_HEIGHTMAP`
env override so the round-trip is one restart:
`WILDERNESS_HEIGHTMAP=heightmap_export.ktx2 cargo run -p
bevy_wilderness_editor_ui --example editor`.

**Phase 9 ✅ — Rebake control (D10.1).** `RebakeSettings { auto }`; when off,
dirty flushes skip the debounce; UI toggle + "Bake" button writing
`RebakeRequested`. *Accept:* with auto off, a sculpt session never re-bakes;
the Bake button re-shades; flipping auto back on restores D5 behavior.
Decisions in flight: `RebakeRequested`/`ClipmapReady` are re-exported from
the editor core so a UI drives bakes without a renderer dependency; flipping
auto *off* cancels an armed debounce, flipping it *on* arms one for every
terrain sitting in clay (else they'd stay stale until the next edit); the
Bake button doubles as the in-flight indicator ("Baking…", disabled).

**Phase 10 ✅ — Clay display mode (D10.2 + D10.3).** Flags-bit clay fallback
(derivative normals + N·L grey) whenever shading is stale, with dynamic
terrain-scale shadows while clay. *Accept:* editing with auto off turns the
terrain readable grey clay (geometry clearly legible while dragging); Bake
returns full shading; a freshly spawned terrain shows clay, not chrome,
until its first bake lands.
Decisions in flight: "never baked" is `ClipmapRvt::ever_baked` (set once on
first bake completion, never reset) — **not** `ClipmapReady`, which is also
absent during every auto-mode *re*-bake and would strobe clay grey on every
stroke; auto mode never shows clay (the previous bake stays on screen);
the mask overlay tint stays visible on clay (masking is part of the
modeling session). Dynamic shadows per D10.3, including the `Mask(0.0)`
alpha-mode coupling documented there.

**Phase 11 ✅ — Stamp tool + library (D11).** Core stamp tool (GPU floating
preview, wheel controls, mask-weighted, toroidal, 8/16-bit PNG) + UI gallery.
*Accept:* pick a stamp from the gallery; the preview floats under the cursor
at full framerate with a half-map-scale stamp; wheel / Ctrl+wheel /
Shift+wheel adjust strength / scale / rotation live; a feathered mask
attenuates it; click commits with **no visible pop** (preview ≡ committed
geometry); Ctrl+Z reverts the whole stamp as one entry; a stamp crossing the
looping seam wraps correctly in preview and commit.
Decisions in flight: renderer API is `Clipmap::stamp: Option<ClipmapStamp>`
(image + center/half-size/rotation/strength/masked), mirrored into the
material per frame — texture at binding 116, transform in the *grown*
`DevParams` uniform, flags bit5/bit6. The composite lives in the **vertex
path of every pipeline**, so in clay mode the preview casts its previewed
shadow. Add/subtract collapsed into **signed strength** (the wheel scrolls
through zero into carving) — no separate mode. `StampData::sample` is the
CPU twin of `textureSampleLevel` (half-integer texel centers, edge clamp),
and the commit is the §10 math-parity gotcha made real: one function, two
implementations, unit-tested. While previewing, the shared pick reads the
*base* terrain (the field is untouched until commit) — accepted in D11.
The wheel is read only while the pick hits terrain, so egui keeps panel
scrolling; the example additionally zeroes the free camera's
`scroll_factor` while the tool is active (it reads the wheel
unconditionally — the D11 ⚠️ input-priority conflict, resolved host-side).
Starter stamps are *generated* (hill/ridge/ring, 16-bit, gitignored) rather
than shipped as binaries.

**Phase 12 (conditional) — Progressive strip re-bake.** Only if measurement
shows the full bake hitches interactively: split the same bake work into N
per-frame strips (the escalation path already blessed in the header note —
no correctness risk, unlike regional). ⚠️ **Measure first** — profile the
actual full-bake cost at default `rvt_size` before building this.

**Phase 13 ✅ — Erosion realism (D12).** Independent of Phase 12; staged as
three shippable commits.
*13a — kill the shot noise:* slope-proportional capacity (floor removed) +
`min_slope_deg` gate, 2-step warm-up, stagnation death, brushed deposition,
quality defaults. *Accept:* erode an untouched map — plains gain gentle
sediment texture, **zero pockmarks**; `flat_plain_never_carved` and
`same_seed_same_result` green.
*13b — flow accumulation:* per-round D8 pass boosts droplet capacity in
channels; progress accounting covers flow passes. *Accept:* the same run
shows **connected dendritic channels**, not scattered scratches;
`flow_concentrates_erosion` green; progress bar still monotonic to 100%.
*13c — deposition realism + maps:* split erode/deposit accumulators,
deposit-only separable blur, `ErosionMaps` behind `keep_maps`. *Accept:*
valley floors and fans read smooth against crisp channel walls;
`deposits_are_smooth` green; `keep_maps` yields plausible wear/deposit/flow
maps.
Decisions in flight: defaults shipped as planned (all three stages
accepted visually at each step). The deposit blur measured ~10× off-hill
prominence reduction (5.5 m → 0.57 m at 8× default density). Plains may
still incise slightly *through their own fans* (streams cutting
floodplains — accepted as realism, bounded by `flat_plain_gains_not_loses`
at <1% of deposition); truly flat terrain is a proven no-op
(`flat_terrain_untouched`). Deposits during a droplet's 2-step warm-up
stay allowed — a fresh droplet has nothing to drop, so the gate only
suppresses carving.

---

## 10. Gotchas (read before coding)

- **R16 precision** — never accumulate erosion in R16; keep the f32 field (P1).
- **Bake is one-shot self-despawning** (`src/rvt.rs:160`) — must be re-triggerable
  (§5.1) or edits never update shading.
- **Unbaked terrain is a chrome mirror** — no cheap "unbaked" look to fall
  back to *(until Phase 10's clay mode lands — which then becomes the
  fallback for every unbaked state)*.
- **`GridMaterial` at the bind-group ceiling** (`src/lib.rs:509`) — don't add
  bindings; pack into `flags`. Precisely: the ceiling is **binding slots**.
  An editing-gated *texture* binding behind a shader def is fine
  (`edit_overlay` precedent), and **growing an existing uniform struct** is
  fine — a *new uniform slot* is what silently breaks (D11 relies on this
  distinction).
- **Stamp preview and commit must share their math** (D11) — the shader
  composite and the CPU apply are two implementations of one function
  (bilinear sample + mask-weighted add); any divergence pops on click.
- **Clay's dynamic shadows lean on bevy internals** (D10.3) — the
  `Mask(0.0)` alpha flip is what gets the material bind group into the
  shadow pipeline. Re-verify on every bevy upgrade; move to
  `PREPASS_READS_MATERIAL` when bevy grows an API for it.
- **Droplet writes scatter** — multithread via per-batch delta buffers, not shared.
- **Erosion determinism is order-fragile** (D12) — batch results merge in
  spawn order, the flow sort tie-breaks on index, the deposit blur is
  single-threaded; `same_seed_same_result` is the tripwire. Also: blur the
  *deposit* accumulator only, and always before mask weighting, or masks
  leak.
- **Mask edge feathering** — skip it → hard rectangular seam.
- **Toroidal everything on looping** — brush footprint, erosion neighbor reads, and
  re-bake all wrap, or the seam breaks.
- **`Heightfield` needs `R16Unorm` + `MAIN_WORLD`** — verify format + asset usage.
- **UI must not leak into the core** (P2) — no egui types in `bevy_wilderness_editor`.
- **`TerrainRegionChanged` from day 1** — retrofitting prop re-snap later is painful.

---

## 11. Open questions

- **Does the full re-bake actually hitch?** Phase 12 is gated on measuring
  the bake's real per-frame cost at default `rvt_size` — don't build the
  strip split on assumption.
- **8-bit stamp terracing** (D11): 256 height levels terrace on tall
  features. Pre-blur/upsample on import, or leave it and let the smooth
  brush handle it? Decide when the tool exists to compare.
- ~~**Export format**~~ *(settled in Phase 8)*: both — KTX2 engine master +
  16-bit PNG interchange, by extension. See D7.
- ~~**Erosion parameter exposure**~~ *(settled in Phase 7)*: five sliders —
  droplet density, sediment capacity, erode/deposit rate, talus angle. The rest
  stay `ErosionSettings` fields a host can still set in code.
- ~~**editor-ui: feature or crate**~~ *(settled in Phase 7)*: a crate, per the
  §7 lean.
