# bevy_wilderness — Roadmap

Where the terrain is going, and why. Each item was reviewed against two
questions: **does a shipped AAA terrain system do this (or something better)?**
and **does it scale from a standalone headset to a high-end desktop?** Items
that failed were revised or dropped (§4 records the verdicts on earlier plans).

How the renderer works today: [architecture.md](architecture.md). Editor:
[editor.md](editor.md).

---

## 1. Goal

Beautiful, nature-shaped terrain — convincing underfoot in VR and stunning
across a vista — from **one asset and one material**, scaling between:

| | **Standalone: Steam Frame** | **Desktop: high-end GPU** |
| --- | --- | --- |
| Hardware | Snapdragon 8 Gen 3 (Adreno 750 class, tile-based mobile GPU, ~10 W), 16 GB LPDDR5X *shared* | Discrete GPU, 12–24 GB VRAM |
| Output | 2 × 2160² per eye, 72–144 Hz (90 Hz typical target) | 1440p–4K flatscreen at 60–144 Hz, or PC VR |
| Terrain budget (target) | ≈ 2 ms GPU per frame, both eyes | ≈ 2–3 ms GPU (Far Cry 5's terrain budget was ~2 ms at full screen) |
| Priorities | stability (no shimmer, no popping), bandwidth, sample count | detail, draw distance, lighting richness |

The terrain is a reusable crate for the game Totem: landscapes are
**user-generated**, so every level must look right from the heightmap and a
small set of rules alone. Per-map data the editor produces — painted layer
weights, masks generated from the heightmap — refines that, never replaces it.

## 2. Principles (AAA practice, applied)

1. **Bake static, sample cheap.** Anything that doesn't change at runtime is
   computed once — in the editor, or per page as the camera first needs it —
   and read with a few fetches. This
   is the virtual-texture lineage — Frostbite's procedural virtual texturing,
   Far Cry's adaptive virtual textures, Unreal's Runtime Virtual Texturing — and
   it is why one material can serve both tiers.
2. **Scale with data, not forks.** Tiers differ in resolutions, distances and
   which optional passes run (`TerrainQuality`), never in a second material.
3. **Stability before sharpness in VR.** The head never stops moving, so
   aliasing reads as shimmer and LOD pops read as the world twitching. Every
   texture is mipped, every LOD change is continuous, specular is
   anti-aliased. A slightly softer image that holds still beats a sharp one
   that sparkles.
4. **Bandwidth is the mobile budget.** On a tile-based mobile GPU, texture
   bytes and fullscreen passes cost more than ALU. Prefer compressed, mipped
   textures, few fullscreen passes, forward rendering with MSAA.
5. **Terrain feeds the world.** In shipped open worlds the terrain's data
   (height, slope, material, shadow) drives vegetation, scatter and object
   blending. Expose it; don't make every system re-derive it.
6. **Measure on the device.** "Efficient enough" is a claim about a Frame, not
   a desktop. Every tier decision below is provisional until profiled there.

## 3. Where we stand

| Capability | AAA practice | bevy_wilderness | Gap |
| ---------- | ------------ | --------------- | --- |
| Geometry LOD | Quadtree/CDLOD or clipmaps, GPU-driven, **geomorphed** transitions | Geometry clipmaps, stitched and geomorphed; coarse rings sample the full-res heightmap | P0 (prefilter) |
| Shading cache | Virtual texture (procedural, streamed, compressed, mipped) | Static full-terrain RVT, baked at load, **no mips, uncompressed** | P1 (sparse virtual texture) |
| Material placement | Procedural rules (slope, height, curvature/flow masks) + painted weights + height blending, 8–16 layers | Slope / height / cavity bands + height blending (4 layers) | P2 (material layer) |
| Tiling | Stochastic/hex tiling, macro-micro detail | Hex tiling in bake + distance-faded detail overlay, top-2 blend | ok |
| Specular aliasing | Toksvig / LEAN-style roughness from normal variance; geometric specular AA | none | P0 |
| Cliffs | Biplanar/triplanar on steep slopes, cliff meshes (FC5 generates them procedurally) | planar only; cliffs stretch | P2 |
| Close-up relief | Tessellation / Nanite displacement / displacement decals | none | P2 (desktop) |
| Terrain shadow | Heightfield/baked for terrain + CSM for dynamic | height-correct shadow-ceiling field for terrain and meshes | ok |
| Static objects → terrain shadow | Baked into lighting | not yet | P2 |
| Terrain GI | Baked sky + bounce, probe volumes for dynamics | none (open-sky ambient); baked input planned | P2 |
| Object ↔ terrain blending | RVT sampled by meshes at ground contact (UE) | not exposed | P2 |
| Vegetation / ground cover | GPU procedural placement (Horizon Zero Dawn), GPU grass (Ghost of Tsushima) | **none** | P2 — biggest visual gap |
| Fog / atmosphere | Height fog + aerial perspective | analytic height fog, inline on every tier; Bevy atmosphere in the example | ok |
| World size | Streamed heightmaps + VT pages | single resident heightmap + RVT (~8 km at 1 m/texel) | P1 (VT pages) / P3 (heightmap streaming) |
| Time of day | Dynamic sun with cached/amortized shadow | fixed sun, baked | P3 |

## 4. Review of earlier plans

| Earlier plan | Verdict | Why |
| ------------ | ------- | --- |
| Per-entity sun-visibility march for flyers | **Done, better** | Replaced by the shadow-ceiling field: one fetch, height-correct for any mesh. A CPU twin is still worth adding (§5 P1). |
| "No realtime shadows of any kind" | **Revised** | Keep for terrain (it never casts per frame). Too strict for dynamic objects on desktop: allow Bevy CSM for characters/props near the camera on the desktop tier; the Frame tier uses blob shadows or one short cascade, decided by profiling. |
| Static-object shadows as a separate crate | **Revised** | The part that matters most — buildings shadowing *terrain* — needs this crate's bake, so it belongs here as an occluder-registration hook (§5 P2). Object-on-object shadows stay the game's (Bevy CSM on desktop, baked/blob on Frame). |
| Full GI system: DDGI probes, per-building volumes, vertex-baked bounce | **Narrowed** | The terrain half (colored sky + one sun bounce baked into the RVT) is cheap and high-value — keep. Leak-free probe volumes for interiors are a game/engine-level system, not terrain; drop from this roadmap. |
| Sky/contact AO channel revival | **Superseded** | The ambient gather is removed; sky occlusion comes from a baked lighting input. Contact AO under objects comes with occluder registration. |
| Triplanar on steep slopes (stretch) | **Promoted** | Cliffs are where heightfield terrain looks cheapest; slope-gated **biplanar** (only cliffs pay) on the desktop tier, cliff meshes for hero rock. |
| Toroidal ring RVT for flatscreen density | **Replaced** | Superseded by the sparse virtual texture (§5 P1), which gives near density at every distance. Don't revive the ring. |
| Offline-baked RVT shipped with the level | **Dropped** | About 720 MB per level at 8192² with mips, for data the client derives from the heightmap and a few small maps. The sparse virtual texture composites pages on demand instead; nothing ships but inputs. |
| Mip-chained whole-world RVT | **Superseded** | Fixed the shimmer but not the memory, near density or shipping; the sparse virtual texture's levels are its mips. |
| Parallax / POM | **Kept, desktop only** | Plus a denser near grid for real displacement (§5 P2). |
| `TerrainQualityKey` + crate feature flags (`hextile`, `rvt`, …) | **Revised** | Runtime presets on `TerrainQuality` instead; compile-time flags would fork the material. |
| Stream source data for planet-scale | **Kept, P3** | Only when Totem needs worlds > ~8 km. |
| HMD-centered clipmap, multiview check, forward+MSAA for VR | **Kept, P1** | Standard VR practice. |
| Shrink/remove the lighting fork | **Kept** | Still re-taxed every Bevy upgrade (0.20 added rect lights). |
| Erosion (droplet, then GPU pipe model) | **Dropped** | Never looked right. Authoring natural landforms is better served by real-world import (already built) and external tools (Gaea, World Machine) exporting 16-bit heightmaps. Revisit only with a proven method (e.g. stream-power + thermal at low resolution, upsampled with detail). |
| Editor Phase 12: progressive strip re-bake | **Kept, conditional** | Only if a full re-bake measurably hitches. |

## 5. Roadmap

### P0 — Stability and correctness (small, high value, both tiers)

- ~~**Mip-chain the RVT.**~~ Folded into the sparse virtual texture (P1):
  each of its levels is composited with derivatives at that level's texel size,
  so the levels are a correctly prefiltered mip chain and ids stay valid.
- ~~**Geomorph LOD transitions.**~~ Done: vertices blend toward the coarser
  ring's surface across each ring's outer band.
- **Prefilter coarse rings.** Rings coarser than the heightmap texel sample it
  at their vertices only (mip 0), so as they snap, distant silhouettes swim:
  undersampling, which geomorphing doesn't address. Give the heightmap a mip
  chain (kept in sync with editor edits) and sample the mip matching each
  ring's cell size.
- **Specular anti-aliasing.** Raise roughness from normal variance (Toksvig-
  style, from the length of the filtered normal) in the VT levels and detail
  normals, plus a geometric term from screen-space normal derivatives (Valve's
  VR rendering talk). Kills sparkle on rough ground.
- **Unbaked fallback for non-editing builds.** Clay exists only with
  `editing`; a shipping build shows chrome until the bake lands. Make clay (or
  a flat neutral shade) the default fallback.
- **RVT edge coverage.** Fade/clamp baked channels past the heightmap
  coverage on finite terrain (the geometry already drops; the shading smears).
- **Use or drop cavity.** It is baked and unused. Use it (cavity darkening of
  albedo / specular occlusion in crevices — cheap realism) or stop baking it.
- **Correctness checks:** grid AABBs when `base_scale ≠ 1`; `Heightfield`
  wrapping for looping terrain.

### P1 — Scaling: one terrain, two tiers

- **Sparse virtual texture** in place of the whole-world RVT. The whole-world
  target has to hold 8 km at the near texel density, so it's too coarse up
  close (1 m/texel at 8192²), too big for standalone memory, and too big to
  ship. A virtual texture holds only what the camera sees, at the density it
  sees it — the Unreal RVT / Far Cry adaptive VT model:

  - **Levels.** Finest texel size is a quality knob (0.5 m desktop, 2 m
    standalone to start); each coarser level halves it, down to one that covers
    the world in a few pages and is always resident, so nothing is unshaded.
  - **Pages** of 256² plus a border for filtering, in a fixed atlas per target
    (4096² desktop ≈ 130 MB for albedo + normal; 2048² standalone ≈ 32 MB).
    Same channels as today's targets.
  - **Residency on the CPU,** from the head position: per level, the pages
    within a radius doubling per level — the clipmap's ring shape, so no GPU
    feedback pass. LRU-cached; revisits are free. Looping terrain wraps page
    coordinates.
  - **Composite per page** with `bake.wesl` as it is, derivatives set to the
    level's texel size, so coarse levels are true averages of the rules rather
    than point samples. A per-frame page budget (a tier knob), coarse first.
  - **Sampling.** Indirection per level (or a quadtree texture); level chosen
    by distance, blended across a band as the geometry rings are; explicit
    derivatives on atlas reads.
  - **Editing** invalidates the pages under a dirty rect: no whole-terrain
    re-bake, and the editor's sculpting path is the runtime path.
  - **Removes** the whole-world bake at load, `rvt_size`, and the mip item.
    The sun shadow field stays whole-world; it's small.
  - **Risks:** page and level seams; pop as fine pages land (sharpening only,
    since coarse is the fine page's average; fade if VR shows it); grazing-angle
    undersampling; composite cost on standalone, unmeasured.
  - **First step:** prototype in the example — residency, per-page composite,
    indirection — and measure composite time per page on both tiers.

- **Named presets.** `TerrainQuality::standalone()` / `::desktop()` (and
  `::low()`), tuned by profiling. Starting point:

  | Knob | Standalone (Frame) | Desktop |
  | ---- | ------------------ | ------- |
  | VT finest texel | 2 m | 0.5 m |
  | VT atlas | 2048² | 4096² |
  | VT pages composited per frame | few | many |
  | `ambient_gather` | on (it's baked; costs one sample) | on |
  | `detail_layers` | 1 | 2 |
  | detail fade distance | short | long |
  | AA | MSAA 4×, forward | TAA or MSAA, deferred ok |
  | biplanar cliffs / POM / dense near grid | off | on |
  | ground cover density & distance | low | high |
  | dynamic-object shadows | blob / one cascade | CSM |

- **Texture memory and compression.** The VT atlas fixes the shading cache's
  memory per tier; compressing pages on the GPU as they're composited is a
  later option, not a need. Ship layer/detail arrays as KTX2 with both BC and
  ASTC variants (Qualcomm recommends ASTC first, ETC2 as fallback), chosen at
  load by device capability.
- **VR rendering path.** Forward + MSAA; clipmap `target` = the HMD head, not
  an eye; verify multiview (both eyes share rings, VT pages and fields — they are
  view-independent by design). Geomorphing measures its band from
  `view.world_position`, i.e. per eye (~3 cm apart, sub-millimetre height
  difference); pass the head position instead so both eyes morph identically. If the Frame's runtime exposes eye-tracked
  foveation (variable-rate shading), use it — terrain fills most of the
  screen and is ideal for it. Verify availability on device.
- **CPU sun-visibility query.** A CPU twin of the shadow-ceiling field for
  gameplay (is the player in shade?) and per-object material params.
- **Device profiling gate.** Before freezing presets, profile on a Frame:
  ms per pass, VT composite time per page, samples per near fragment against the ~16-texture
  ceiling, and thermal throttling over a 20-minute session.

### P2 — Visual pillars

- **Material layer** — most of what reads as real up close. Up to 16 layers
  (per-layer params leave their `Vec4` lanes); placement from today's rules,
  plus **painted weight maps** from the editor (small, a few m/texel, four
  layers per RGBA) and **heightmap masks** (flow, wetness, wear, deposition)
  as new rule kinds, all composited into VT pages; height blending across every
  layer; a low-frequency macro tint so tiling never shows from the air. Scanned
  PBR sets at 4K desktop / 1K ASTC standalone.
- **Ground cover and scatter** (biggest gap). GPU procedural placement driven
  by the same slope/height bands and baked material ids (the Horizon Zero
  Dawn model: deterministic, data-driven, regenerated around the player), with
  GPU-generated grass blades near the camera (Ghost of Tsushima), instanced
  meshes mid-range and impostors far. Density and distance are tier knobs.
  Probably its own crate (`bevy_wilderness_foliage`) consuming the terrain's
  public data: `Heightfield`, material ids, the shadow field.
- **Terrain GI.** An optional baked lighting input (colored sky irradiance,
  bounce off sunlit slopes into shadowed valleys, sun visibility with static
  structures in it) that replaces the open-sky ambient and the field's ground
  shadow. Baked outside the crate, by the host; zero runtime cost; the
  biggest step from "lit" to "grounded".
- **Static occluders shadow the terrain.** A registration hook: static meshes
  (buildings, rock formations, trees as proxies) are rendered from the sun
  during the bake into the shadow-ceiling field, with
  contact AO under them. Height-correct for flyers via the field.
- **Object–terrain blending.** Expose the VT (albedo, normal) so rocks,
  buildings and props sample it near their ground contact and blend into the
  terrain seamlessly (Unreal's RVT mesh blending). Cheap: the pages exist
  wherever an object is near the camera.
- **Curvature-driven material response.** Use the baked cavity as a material
  mask, not an occlusion term. *Placement is done* (`CavityRule`; the example
  puts gravel sediment in gullies). Remaining: a darker, smoother "damp"
  surface response in hollows, and exposed weathered ridges. Standard in
  terrain tools (Gaea and World Machine curvature/flow masks). It was briefly
  applied to ambient occlusion and removed: measured on the example map it
  darkened deep gullies to ~40 % where the physically based horizon AO gave
  ~83 %, and 71 % of its darkening landed where the AO saw ≥ 90 % open sky.
- **Cliffs.** Slope-gated biplanar mapping in the VT composite (every tier —
  each page is composited once) and in the near detail overlay (desktop;
  flat ground stays single-planar), and authored/procedural cliff
  meshes for hero rock (Far Cry 5's procedural cliff pipeline uses slope as
  its input).
- **Close-up relief (desktop).** wgpu has no tessellation shaders, so: a finer
  level-0 grid near the camera displaced by the detail height of the dominant
  material, plus POM for the remaining near rock. Far Cry 5 shipped
  displacement *decals* instead of tessellation for cost — a good fit for
  rocks, roots and paths.

### P3 — Scale and dynamics

- **Bigger worlds.** Tiled heightmap streaming once Totem needs worlds beyond
  ~8 km. The sparse virtual texture (P1) already pages the shading.
- **Time of day.** Amortized re-bake of sun-vis and the shadow field as the
  sun moves (budgeted strips per frame), never live per-pixel marching.
- **Lighting fork.** Replace `terrain_apply_lighting` with stock Bevy lighting
  plus a per-pixel directional shadow factor if Bevy grows one; until then keep
  it minimal and re-sync it each upgrade.

### Editor

- Progressive strip re-bake (Phase 12) — only if a full re-bake measurably
  hitches while editing.
- 8-bit stamps terrace on tall features: pre-blur on import.
- Tests for the ECS systems (cursor, dirty sync, re-bake debounce).
- Landform authoring now that erosion is gone: lean on real-world import and
  external-tool heightmaps; consider a terrain-noise "generate" tool
  (ridged multifractal + domain warp) for quick base shapes.

## 6. Housekeeping

Stale comments found while writing these docs:

- `world.rs` module doc and the UI world section describe only AWS tiles (USGS
  and Auto exist).
- `export.rs` and `sync_clay_shadows` mention Bevy 0.19.
- `ClipmapStamp` doc says "red channel (0..1)"; the editor supplies `R16Snorm` −1..1.
- `terrain.wesl` flags comment lists bits 0–2 only; `TerrainQuality::rvt_size`
  says "three targets" (there are four); `warn_late_quality` omits
  `sun_shadow_size`.
- `TerrainGesture::open()` has no callers since erosion was removed.
- The example header doesn't list Alt+wheel (stamp feather).

## 7. References

- Steam Frame announcement & specs — [UploadVR](https://uploadvr.com/valve-steam-frame-official-announcement-features-details), [Road to VR](https://roadtovr.com/valve-steam-frame-release-date-price-specs/)
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
