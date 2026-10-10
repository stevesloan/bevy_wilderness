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
**user-generated**, so nothing may depend on hand-painted per-map data —
placement, shading and lighting must derive from the heightmap and a small set
of rules.

## 2. Principles (AAA practice, applied)

1. **Bake static, sample cheap.** Anything that doesn't change at runtime is
   computed once (at load or in the editor) and read with a few fetches. This
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
| Geometry LOD | Quadtree/CDLOD or clipmaps, GPU-driven, **geomorphed** transitions | Geometry clipmaps, stitched, **no morphing** | P0 |
| Shading cache | Virtual texture (procedural, streamed, compressed, mipped) | Static full-terrain RVT, baked at load, **no mips, uncompressed** | P0 / P1 (offline bake) |
| Material placement | Procedural rules (slope, height, curvature/flow masks) + height blending | Slope / height / cavity bands + height blending (4 layers) | ok |
| Tiling | Stochastic/hex tiling, macro-micro detail | Hex tiling in bake; distance-faded detail overlay with per-pixel height blend, hex-tiled on desktop | ok |
| Specular aliasing | Toksvig / LEAN-style roughness from normal variance; geometric specular AA | none | P0 |
| Cliffs / rock | Triplanar on steep slopes; rock as scanned meshes (Death Stranding, FC5 procedural cliffs) | triplanar in bake + near detail; no rock meshes | P2 (rock meshes) |
| Mid-scale relief (1–10 m) | Per-material macro detail height / normals | Procedural `Relief` per layer (noise + ledges), baked into normals + near-field shadow | ok |
| Close-up relief | Tessellation / Nanite displacement / POM / displacement decals | none | P2 (desktop) |
| Contact AO | GTAO / screen-space AO | baked macro AO + detail AO + micro-shadows | P2 (desktop SSAO) |
| Terrain shadow | Heightfield/baked for terrain + CSM for dynamic | baked sun-vis + height-correct shadow-ceiling field for meshes | ok |
| Static objects → terrain shadow | Baked into lighting | not yet | P2 |
| Terrain GI | Baked sky + bounce, probe volumes for dynamics | macro AO + bent normal (monochrome) | P2 |
| Object ↔ terrain blending | RVT sampled by meshes at ground contact (UE) | not exposed | P2 |
| Scatter: rock meshes, vegetation | GPU procedural placement (Horizon Zero Dawn / Death Stranding), GPU grass (Ghost of Tsushima) | **none** | P2 — biggest visual gap |
| Fog / atmosphere | Height fog + aerial perspective | analytic height fog, two tiers; Bevy atmosphere in the example | ok |
| Source elevation | 1–2 m lidar / DEM + erosion tools | USGS 3DEP 1 m (US) + ~30 m global tiles; 4096² editor maps (2 m) | P1 (sources) |
| World size | Streamed heightmaps + VT pages | single resident heightmap + RVT (~8 km at 1 m/texel) | P3 |
| Time of day | Dynamic sun with cached/amortized shadow | fixed sun, baked | P3 |

## 4. Review of earlier plans

| Earlier plan | Verdict | Why |
| ------------ | ------- | --- |
| Per-entity sun-visibility march for flyers | **Done, better** | Replaced by the shadow-ceiling field: one fetch, height-correct for any mesh. A CPU twin is still worth adding (§5 P1). |
| "No realtime shadows of any kind" | **Revised** | Keep for terrain (it never casts per frame). Too strict for dynamic objects on desktop: allow Bevy CSM for characters/props near the camera on the desktop tier; the Frame tier uses blob shadows or one short cascade, decided by profiling. |
| Static-object shadows as a separate crate | **Revised** | The part that matters most — buildings shadowing *terrain* — needs this crate's bake, so it belongs here as an occluder-registration hook (§5 P2). Object-on-object shadows stay the game's (Bevy CSM on desktop, baked/blob on Frame). |
| Full GI system: DDGI probes, per-building volumes, vertex-baked bounce | **Narrowed** | The terrain half (colored sky + one sun bounce baked into the RVT) is cheap and high-value — keep. Leak-free probe volumes for interiors are a game/engine-level system, not terrain; drop from this roadmap. |
| Sky/contact AO channel revival | **Folded in** | Ambient gather already bakes AO + bent normal. Contact AO under objects comes with occluder registration. |
| Triplanar on steep slopes (stretch) | **Done** | Triplanar in the bake (always) and the near detail (`detail_triplanar`, desktop). What remains for rock is meshes (P2). |
| Toroidal ring RVT for flatscreen density | **Replaced** | Superseded by mip-chained RVT + a streamed/adaptive virtual texture when worlds outgrow one RVT (P3). Don't revive the ring. |
| Parallax / POM | **Kept, desktop only** | Plus a denser near grid for real displacement (§5 P2). |
| `TerrainQualityKey` + crate feature flags (`hextile`, `rvt`, …) | **Revised** | Runtime presets on `TerrainQuality` instead; compile-time flags would fork the material. |
| Stream source data for planet-scale | **Kept, P3** | Only when Totem needs worlds > ~8 km. |
| HMD-centered clipmap, multiview check, forward+MSAA for VR | **Kept, P1** | Standard VR practice. |
| Shrink/remove the lighting fork | **Kept** | Still re-taxed every Bevy upgrade (0.20 added rect lights). |
| Erosion (droplet, then GPU pipe model) | **Dropped** | Never looked right. Authoring natural landforms is better served by real-world import (already built) and external tools (Gaea, World Machine) exporting 16-bit heightmaps. Revisit only with a proven method (e.g. stream-power + thermal at low resolution, upsampled with detail). |
| Editor Phase 12: progressive strip re-bake | **Kept, conditional** | Only if a full re-bake measurably hitches *in the editor*. For shipping, the bake moves offline (P1). |
| Real-scale texture sets (macro aerial + ground-level detail per layer) | **Tried, reverted** | Measured: the example's scans are used 1.7–50× off their real size. A two-set, true-scale version was built and rejected on look; the original textures stay. Revisit per texture, not wholesale. |
| Replacement rock textures (`rock_ground_02`, `rocks_ground_02`, `cliff_side`) | **Compared, kept original** | Side-by-side renders on the same cliff; the original `marble_cliff_05` stays. Rock realism up close comes from meshes, not the terrain texture. |

## 5. Roadmap

### P0 — Stability and correctness (small, high value, both tiers)

- **Mip-chain the RVT.** Targets are single-level today, so distant terrain
  samples an 8192² texture with no mips: shimmer (worst in VR), texture
  cache thrash (worst on mobile), and material boundaries that alias into a
  hard line at distance instead of averaging into a soft transition.
  Bevy 0.20's `MipGenerationJobs` downsampler doesn't fit: it can't write
  sRGB targets, and box-averaging corrupts the packed material ids. Preferred:
  **bake each mip level directly** (one bake camera per level into a mip
  view) — the bake's `textureSampleGrad` then prefilters the source arrays
  correctly at every level, and ids stay valid. ~⅓ extra bake time. With the
  offline bake (P1) the mips can be built at export instead, with per-channel
  filters (renormalized normals, majority ids).
- **Geomorph LOD transitions.** Blend each vertex toward its coarser-level
  position across the outer band of every ring (GPU Gems 2 clipmaps / CDLOD
  morphing) so resolution changes are continuous instead of popping.
- **Specular anti-aliasing.** Raise roughness from normal variance (Toksvig-
  style, from the length of the filtered normal) in the RVT mips and detail
  normals, plus a geometric term from screen-space normal derivatives (Valve's
  VR rendering talk). Kills sparkle on rough ground — more pressing now that
  rock has a sheen (roughness 0.8).
- **Unbaked fallback for non-editing builds.** Clay exists only with
  `editing`; a shipping build shows chrome until the bake lands. Make clay (or
  a flat neutral shade) the default fallback.
- **RVT edge coverage.** Fade/clamp baked channels past the heightmap
  coverage on finite terrain (the geometry already drops; the shading smears).
- **Correctness checks:** grid AABBs when `base_scale ≠ 1`; `Heightfield`
  wrapping for looping terrain.

### P1 — Scaling: one terrain, two tiers

- **Offline bake: export and load.** The bake will be pre-rendered rather
  than run on the headset. Export the RVT targets and the sun shadow-ceiling
  field (KTX2, mipped, block-compressed: ASTC for the Frame, BC for desktop)
  from the editor alongside the heightmap; the runtime loads them instead of
  baking. Absorbs RVT mips and texture compression, and makes bake cost a
  build-time concern — today's bake (relief, cavity, breakup, triplanar)
  already runs long enough to stall a desktop frame. The sun is fixed at
  export, as it is at bake today. Editing keeps the runtime bake.
- **Named presets.** `TerrainQuality::standalone()` / `::desktop()` (and
  `::low()`), tuned by profiling. Starting point:

  | Knob | Standalone (Frame) | Desktop |
  | ---- | ------------------ | ------- |
  | `rvt_size` | 4096² | 8192² |
  | RVT compression | ASTC / ETC2 | BC1 / BC5 |
  | `ambient_gather` | on (it's baked; costs one sample) | on |
  | `detail_layers` | 2 (1 drops the per-pixel height blend; boundary pixels only) | 2 |
  | detail fade distance | short | long |
  | fog | `Low` (inline) | `High` (fullscreen) |
  | AA | MSAA 4×, forward | TAA or MSAA, deferred ok |
  | `detail_hex_tiling` | off | on |
  | `detail_triplanar` | off (bake stays triplanar) | on |
  | POM / dense near grid / SSAO | off (maybe one-step parallax ≤ 3 m, if profiling allows) | on |
  | ground cover density & distance | low | high |
  | dynamic-object shadows | blob / one cascade | CSM |

- **Texture memory and compression.** Uncompressed RGBA8 8192² RVT targets are
  ~270 MB each (~1 GB for three with mips). Compress the RVT once after baking
  (GPU block compression — BC on desktop; ASTC on Adreno, where Qualcomm
  recommends ASTC first and ETC2 as fallback). Ship layer/detail arrays as
  KTX2 with both BC and ASTC variants, chosen at load by device capability.
- **VR rendering path.** Forward + MSAA; clipmap `target` = the HMD head, not
  an eye; verify multiview (both eyes share rings, RVT and fields — they are
  view-independent by design). If the Frame's runtime exposes eye-tracked
  foveation (variable-rate shading), use it — terrain fills most of the
  screen and is ideal for it. Verify availability on device.
- **CPU sun-visibility query.** A CPU twin of the shadow-ceiling field for
  gameplay (is the player in shade?) and per-object material params.
- **More elevation sources.** World import offers USGS 3DEP 1 m (US) and
  ~30 m global tiles. Add: Copernicus GLO-30 (better global 30 m, COGs on
  AWS), Norway DTM1 (1 m, CC BY 4.0), England LIDAR DTM (1 m), swissALTI3D,
  ArcticDEM (2 m, north of 60° incl. Iceland; the 2 m product is licensed for
  research/education), New Zealand LINZ (1 m regional). Use bare-earth DTMs.
  And 8192² editor maps: the default 4096² over 8 km is 2 m/texel, so 1 m
  lidar is averaged away. With lidar-resolution maps, `Relief` depth can drop.
- **Device profiling gate.** Before freezing presets, profile on a Frame:
  ms per pass, RVT memory, samples per near fragment against the ~16-texture
  ceiling, and thermal throttling over a 20-minute session.

### P2 — Visual pillars

- **Scatter: rock meshes first, then ground cover** (biggest gap). Up close,
  rock in AAA games is scanned meshes, not terrain texture (Death Stranding:
  photogrammetry + GPU procedural placement; ~25 M polygons in one DS2 scene).
  GPU procedural placement driven by the same slope / height / cavity bands
  and baked material ids (the Horizon Zero Dawn model: deterministic,
  data-driven, regenerated around the player). **Rock first:** boulders and
  outcrops on rock areas, sunk into the terrain and blended via the RVT;
  on the Frame, a few low-poly hero rocks with baked normal maps, aggressive
  LOD and impostors. **Then ground cover:** GPU grass near the camera (Ghost
  of Tsushima), instanced meshes mid-range, impostors far. Density and
  distance are tier knobs. Probably its own crate (`bevy_wilderness_scatter`
  or `_foliage`; **open decision**) consuming the terrain's public data:
  `Heightfield`, material ids, the shadow field.
- **Terrain GI.** Extend the ambient gather from monochrome sky occlusion to
  colored sky irradiance + a single sun bounce (light reflected off sunlit
  slopes into shadowed valleys), baked into the RVT. Zero runtime cost; the
  biggest step from "lit" to "grounded".
- **Static occluders shadow the terrain.** A registration hook: static meshes
  (buildings, rock formations, trees as proxies) are rendered from the sun
  during the bake into the sun-vis channel and the shadow-ceiling field, with
  contact AO under them. Height-correct for flyers via the field.
- **Object–terrain blending.** Expose the RVT (albedo, normal) so rocks,
  buildings and props sample it near their ground contact and blend into the
  terrain seamlessly (Unreal's RVT mesh blending). Cheap: the RVT exists.
- **Curvature-driven material response.** Use the baked cavity as a material
  mask, not an occlusion term. *Placement is done* (`CavityRule`; the example
  puts gravel sediment in gullies). Remaining: a darker, smoother "damp"
  surface response in hollows, and exposed weathered ridges. Standard in
  terrain tools (Gaea and World Machine curvature/flow masks). It was briefly
  applied to ambient occlusion and removed: measured on the example map it
  darkened deep gullies to ~40 % where the physically based horizon AO gave
  ~83 %, and 71 % of its darkening landed where the AO saw ≥ 90 % open sky.
- **Close-up relief (desktop).** wgpu has no tessellation shaders, so: a finer
  level-0 grid near the camera displaced by the baked `Relief` function (so
  geometry matches the relief shading) plus the detail height of the dominant
  material, plus POM **with self-shadowing** for the remaining near rock. Far
  Cry 5 shipped displacement *decals* instead of tessellation for cost — a
  good fit for rocks, roots and paths. The Frame might afford a one-step
  parallax within ~3 m; profile first.
- **Screen-space AO (desktop).** Bevy's SSAO for small-scale contact
  darkening; a deliberate desktop-only exception to "bake everything" (a
  fullscreen pass is expensive on a tiled mobile GPU).
- **Micro-shadowing for local lights.** It covers the sun only; point and
  spot lights (torches) would gain the same crevice depth. Terrain also
  doesn't receive Bevy 0.20's rect area lights.

### P3 — Scale and dynamics

- **Bigger worlds.** Tiled heightmap streaming + an adaptive/streamed virtual
  texture (Far Cry 4's sector-based adaptive VT) once Totem needs worlds
  beyond ~8 km.
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
- `assets/stamps/Erosion2_Out_8K.png` in the editor UI crate is a leftover from
  the erosion work.

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
- [Normal Mapping for a Triplanar Shader — Ben Golus](https://bgolus.medium.com/normal-mapping-for-a-triplanar-shader-10bf39dca05a)
- Death Stranding: [PC Gamer on Decima](https://www.pcgamer.com/games/action/13-year-old-decima-engine-allows-death-stranding-team-to-accomplish-many-things-that-would-be-difficult-to-achieve-elsewhere/), [Kojima on photogrammetry](https://ag.hyperxgaming.com/article/2256/kojima-talks-photogrammetry-in-death-stranding)
- Elevation data: [ArcticDEM on AWS](https://registry.opendata.aws/pgc-arcticdem), [Copernicus DEM on AWS](https://forum.sentinel-hub.com/t/copernicus-dem-data-available-on-aws/3027), [Norway DTM1](https://data.norge.no/en/datasets/1a7327eb-1fa5-3432-8dea-fc198a5ede13/hoyde-dtm1), [England LIDAR DTM](https://ckan.publishing.service.gov.uk/dataset/01b3ee39-da3f-47b6-83da-dc98e73a461f), [swissALTI3D](https://ckan.opendata.swiss/dataset/swissalti3d/resource/c2d9dbe8-18ea-4ac5-972e-ba69d0b37706)
