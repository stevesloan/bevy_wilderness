# bevy_wilderness editor — Architecture

How the terrain editor works today. The editor is a **framework, not an app**:
a Bevy plugin with a UI-agnostic API that a game embeds (with its own UI and
its own tools), plus an optional reference egui UI.

| Crate | Role |
| ----- | ---- |
| `crates/bevy_wilderness_editor` | Editor core: data model, tools, undo, bake control, import/export. No UI dependency. |
| `crates/bevy_wilderness_editor_ui` | Default egui panel driving the core's public API. Optional and replaceable. Hosts `examples/editor.rs`. |

The editor is a **desktop authoring tool**. It is never run in VR; its output
(a heightmap) is what has to respect the runtime budget. That frees it to use
desktop RAM and threads.

Renderer internals referenced here are described in
[architecture.md](architecture.md).

---

## 1. Principles

- **P1 — f32 authoritative field, derived R16 display map.** All edits happen
  in a CPU `f32` field; the GPU heightmap (R16Unorm) is re-quantized from it per
  dirty region. Never accumulate edits in R16: one step over a ±1312 m range is
  ≈ 4 cm, so gentle brush deltas would round to zero.
- **P2 — No UI in the core.** No egui types in `bevy_wilderness_editor`. A UI
  talks to the core only through resources and messages.
- **P3 — Extension points.** Tools are registered by id; a host tool is just a
  system gated on `tool_active(id)` in `EditorSet::Tools`. The example's
  prop-placement tool proves the path a game's glTF-placement tool uses.
- **Toroidal everything.** On looping terrain every footprint, read and
  overlay wraps; a stroke across the seam is continuous.

## 2. Embedding

```rust
app.add_plugins((ClipmapPlugin, TerrainEditorPlugin, TerrainEditorUiPlugin::default()));
// mark the terrain editable:
commands.entity(clipmap).insert(Editable);
```

- `TerrainEditorUiPlugin::default()` draws its own left panel.
- `TerrainEditorUiPlugin { embedded: true }` draws nothing; the host takes the
  `TerrainUi` system param and calls the sections it wants (`brush_section`,
  `stamp_section`, …). Tool-specific sections hide themselves for other tools,
  so hosts can call them unconditionally.
- A UI writes `PointerBlocked` while it owns the pointer, so tools don't paint
  through panels. That is the entire UI ↔ core input handshake.

## 3. Data model

**`TerrainField`** (`field.rs`) — row-major `f32` heights in meters plus
dimensions, `texel_size`, the R16 encode range (`min`/`max`, same as
`Clipmap`), and `looping`.

- Wrap helpers: `get` wraps (looping) or edge-clamps (finite) like the shaders;
  `wrap_texel` / `wrap_rect` split a footprint into ≤ 4 in-bounds pieces across
  the seam, or drop overhang on finite maps. **Every footprint goes through
  them**; `mark_dirty` and undo capture require in-bounds rects.
- `height_at` (bilinear, matches the shader and renderer `Heightfield`),
  `raycast` (clearance-scaled march + bisection), world ↔ texel mapping.
- Quantization: `to_image`, `to_r16`, `write_region(image, rect)`.

**`EditableTerrain`** (component, `terrain.rs`) — the field, the **mask**
(`f32` 0..1 per texel, with a non-zero count so "no mask painted" means
"everything editable"), and dirty-rect lists for heights and mask. Dirty rects
are kept as a list, not unioned (a seam-crossing footprint dirties opposite
edges; their union would be the whole map).

**Mask overlay.** An `R8Unorm` image assigned to `Clipmap.edit_overlay`; the
renderer tints it orange. Mask flushes don't re-bake.

**`TerrainHeight`** (system param) — `sample(xz)` for host tools snapping props.

## 4. Schedule

`EditorSet::{Pick, Tools, Apply}` run chained in `Update`:

1. **Before Pick** — terrain swaps (new / load / finished world import), then
   initialization of newly `Editable` terrains and their overlays.
2. **Pick** — `update_terrain_cursor`: raycasts from the clipmap's `target`
   camera against the f32 field into `TerrainCursor` (empty when
   `PointerBlocked`).
3. **Tools** — built-in tools and host tools mutate the field / mask and mark
   dirty rects.
4. **Between Tools and Apply** — undo/redo requests; world-import start.
5. **Apply** — `sync_dirty_regions` re-quantizes dirty rects into the R16
   heightmap (one GPU re-upload), emits `TerrainRegionChanged` per rect, and
   then either arms the re-bake debounce (auto mode) or switches the terrain to
   clay (manual mode). `sync_dirty_masks` updates the overlay.
6. **After Apply** — the 200 ms re-bake debounce ticks; on expiry the terrain
   gets `RebakeRequested`.

A sculpt stroke therefore deforms geometry the next frame (vertex shader reads
the new heightmap), and shading catches up one debounce + bake later.

## 5. Tools

**Registry.** `EditorTools` (id → name), `ActiveTool`, `ToolId` (built-ins
`SCULPT`, `MASK`, `STAMP`), `tool_active(id)`. Registering is data only;
behavior is the tool's own systems.

**Cursor.** `TerrainCursor(Option<TerrainHit { terrain, position, texel }>)`.
`texel` may lie outside the map on a repeat tile; the wrap helpers resolve it.
`BrushRing` toggles the gizmo ring.

**Sculpt** (`sculpt.rs`). `BrushSettings { mode, radius, strength }`;
`SculptMode::{Raise, Lower, Smooth, Flatten}`. Held Shift = smooth, Ctrl =
invert (raise ↔ lower, smooth → sharpen, flatten → exaggerate). Smoothstep
falloff × mask weight. Smooth relaxes toward a separable box blur computed
before any writes (order-independent); flatten targets the height at stroke
start. All results clamp to the encode range.

**Mask** (`mask.rs`). Feathered paint, Shift erases; undoable on its own buffer.
Sculpt and stamp both weight their deltas by it.

**Stamp** (`stamp.rs`). `ActiveStamp(Option<StampData>)` +
`StampSettings { size, strength, rotation, feather, offset }`. A grayscale PNG
(8- or 16-bit) is baked into signed heights (baseline removed, offset, edge
feather).

- **Preview is GPU-side:** `drive_stamp_tool` writes `Clipmap.stamp`; the
  terrain vertex shader composites the stamp on top of the heightmap, so even a
  half-map stamp previews at full framerate with zero CPU cost.
- **Commit is CPU-side:** one click applies the same math (`StampData::sample`
  mirrors `textureSampleLevel`: half-integer texel centers, edge clamp) into
  the field as one undo entry. ⚠️ Preview and commit are two implementations of
  one function — change both or the terrain pops on click.
- Wheel = strength (crosses zero into carving), Ctrl = size, Shift = rotate,
  Alt = feather. The wheel is only read while the pick hits terrain.

## 6. Undo

`UndoHistory` is a generic stack of `Box<dyn UndoAction>` capped by total bytes
(default 256 MiB; the newest entry is never evicted). Hosts push their own
actions (e.g. prop placement) into the same stream.

Terrain edits use **tile snapshots**: `TerrainGesture::begin` opens an entry;
`capture(terrain, buffer, rect)` — called **before** mutating — copies each
64² tile (`UNDO_TILE_SIZE`, ≈ 16 KB) the first time it's touched; `seal` pushes
it. Undo/redo paste tiles back and mark them dirty, so re-quantize, prop
re-snap and re-bake all come from the normal Apply path. Buffers:
`UndoBuffer::{Height, Mask}`.

Messages: `UndoRequest`, `RedoRequest` → `UndoApplied { label, redone }`.

## 7. Bake control and clay

- `RebakeSettings { auto }` (default on): dirty flushes arm a 200 ms debounce,
  then `RebakeRequested`. The previous bake stays on screen meanwhile.
- With auto off, edits switch the terrain to **clay** (grey, derivative
  normals, terrain-scale CSM shadows — see architecture.md §8) until the user
  bakes. Rationale: "geometry is true, materials are pending" reads coherently;
  old rock texture stretched over a new mountain does not.
- `clear_clay_on_bake` leaves clay when `ClipmapReady` returns.

## 8. New, load, export

- `NewTerrainRequested { terrain, size, height }` — flat field at a new
  resolution, same world footprint and encode range.
- `LoadRequested { terrain, path }` → `TerrainLoaded { …, error }` — reads
  KTX2 (R16_UNORM) or 16-bit PNG synchronously from the filesystem (absolute
  paths work, for file dialogs). Failure leaves the terrain untouched.
- Every swap clears undo history and aborts in-flight imports (their buffers
  match the old field).
- `ExportRequested { terrain, path }` → `HeightmapExported` — async;
  `.png` writes 16-bit grayscale (interchange, e.g. physics colliders),
  anything else a hand-written R16Unorm KTX2 (engine master).

## 9. Real-world import

`WorldImportRequested { terrain }` with `WorldImportSettings { latitude,
longitude, vertical_scale, source, seamless }` fills the map with real Earth
elevation centered on a point. Runs on `AsyncComputeTaskPool`
(`WorldImportRun::progress()`), lands as `WorldImported { error, note }`.

| Source | Data | Notes |
| ------ | ---- | ----- |
| `Global` | AWS Open Data terrarium tiles (`world.rs`) | worldwide, ~30 m; zoom picked from texel size, ≤ 400 tiles |
| `Usa1m` | USGS 3DEP 1 m lidar (`usgs.rs`, `cog.rs`, `utm.rs`) | discovered via the National Map API; Cloud-Optimized GeoTIFFs read by HTTP range; UTM projection |
| `Auto` | lidar if coverage ≥ 98 %, lidar + global gap fill (datum-matched) if ≥ 2 %, else global | default |

`seamless` (looping terrain only) makes the crop tile without a seam using the
periodic-plus-smooth decomposition (Moisan 2011, FFT Poisson solve,
`periodic.rs`).

## 10. Seam overlay

`SeamOverlay { enabled }` draws a gizmo ring around the tile instance under the
camera on looping terrain — the wrap line is invisible by design.

## 11. Tests

Unit-tested: field math and wrap helpers, sculpt ops, stamp sampling/commit,
mask, undo and gestures, export/load round trips, world import (decode,
resample, datum blend, gap fill, seamless), COG/USGS parsing, UTM.
Not tested: the ECS systems themselves (cursor, sync, re-bake), the UI crate.
Two live-network tests are `#[ignore]`.
