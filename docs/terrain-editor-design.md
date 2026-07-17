# Terrain Editor Framework — Design Doc

Status: **All phases (0–8) complete** — workspace + editing API; editor core;
sculpt; debounced re-bake; undo/history; feathered mask + overlay
visualization; background droplet + thermal erosion; looping seams + boundary
overlay; default egui UI + prop-placement demo tool; KTX2 + 16-bit PNG
export · Project name: **`bevy_wilderness`** · Last updated: 2026-07-17

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
Cursor→terrain via the shared raycast. Edit f32 field → re-quantize dirty region
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
  path). Also opens in DCC tools.

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

---

## 10. Gotchas (read before coding)

- **R16 precision** — never accumulate erosion in R16; keep the f32 field (P1).
- **Bake is one-shot self-despawning** (`src/rvt.rs:160`) — must be re-triggerable
  (§5.1) or edits never update shading.
- **Unbaked terrain is a chrome mirror** — no cheap "unbaked" look to fall back to.
- **`GridMaterial` at the bind-group ceiling** (`src/lib.rs:509`) — don't add
  bindings; pack into `flags`.
- **Droplet writes scatter** — multithread via per-batch delta buffers, not shared.
- **Mask edge feathering** — skip it → hard rectangular seam.
- **Toroidal everything on looping** — brush footprint, erosion neighbor reads, and
  re-bake all wrap, or the seam breaks.
- **`Heightfield` needs `R16Unorm` + `MAIN_WORLD`** — verify format + asset usage.
- **UI must not leak into the core** (P2) — no egui types in `bevy_wilderness_editor`.
- **`TerrainRegionChanged` from day 1** — retrofitting prop re-snap later is painful.

---

## 11. Open questions

- ~~**Export format**~~ *(settled in Phase 8)*: both — KTX2 engine master +
  16-bit PNG interchange, by extension. See D7.
- ~~**Erosion parameter exposure**~~ *(settled in Phase 7)*: five sliders —
  droplet density, sediment capacity, erode/deposit rate, talus angle. The rest
  stay `ErosionSettings` fields a host can still set in code.
- ~~**editor-ui: feature or crate**~~ *(settled in Phase 7)*: a crate, per the
  §7 lean.
