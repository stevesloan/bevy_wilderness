# Bevy Wilderness

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

Beautiful, up close and from afar, nature-shaped terrain for Bevy.

Forked from [bevy-clipmap](https://github.com/kirillsurkov/bevy-clipmap) by
Kirill Surkov — thanks! The fork has diverged heavily (baked RVT shading,
procedural splat, height fog, toroidal looping, editable-terrain API) and lives
on under a new name.

![Screenshot](screenshot.png)

## Overview

The renderer implements GPU-Based Geometry Clipmaps from this paper: https://hhoppe.com/gpugcm.pdf

This is an adaptive LOD technique that allows us to render huge worlds for cheap!

### Features

- **Geometry clipmap terrain** — adaptive LOD for huge worlds.
- **Procedural multi-layer material** — splat blending placed by slope/height bands.
- **Baked shading (RVT)** — albedo, normals, roughness, self-shadow, macro AO /
  bent normal / cavity, plus a near-range detail overlay.
- **Height fog** with High/Low quality tiers (flatscreen + standalone VR).
- **Toroidal looping** — optionally tile the heightmap for seamless infinite terrain.
- **Editable-terrain API** (`editing` feature) — a `RebakeRequested` re-bake
  trigger and the CPU `Heightfield` query, the hooks the terrain editor builds on
  (see [docs/terrain-editor-design.md](docs/terrain-editor-design.md)).

## Usage

The example's terrain material uses CC0 texture sets that are not committed to the
repo. Fetch them once before running it:

```sh
> pip install Pillow
> python crates/bevy_wilderness/assets/fetch_textures.py   # downloads grass/dirt/rock/snow into crates/bevy_wilderness/assets/terrain/
> cargo run --example basic
```

To use your own textures, load one image file per layer with `load_terrain_array`
(albedo `srgb=true`; normal and ORM `srgb=false`, where ORM packs occlusion,
roughness, metallic into R, G, B). See `examples/basic.rs`.

Normal maps must be **OpenGL convention** (+Y / green points up), like Poly Haven's
`nor_gl` set. If crevices and cracks look like raised bumps or veins, your normals
are DirectX convention — invert the green channel to convert them.

## Terrain editor

The workspace includes an embeddable terrain editor:
[`bevy_wilderness_editor`](crates/bevy_wilderness_editor) (the UI-agnostic
core a game embeds with its own UI) and
[`bevy_wilderness_editor_ui`](crates/bevy_wilderness_editor_ui) (an optional
egui side panel driving that same public API). Design and decisions live in
[docs/terrain-editor-design.md](docs/terrain-editor-design.md).

Launch the editor app (uses the same textures as `basic` — fetch them once,
see above):

```sh
> cargo run -p bevy_wilderness_editor_ui --example editor
```

It opens on a fresh flat 4096² terrain. Fly with WASD + right-drag; everything
is driven from the side panel, with keyboard shortcuts mirroring it (see the
doc comment atop
[`examples/editor.rs`](crates/bevy_wilderness_editor_ui/examples/editor.rs)
for the full list):

- **Sculpt** (S) — raise / lower / smooth / flatten under an adjustable brush.
- **Mask** (M) — paint a feathered mask that confines sculpting and erosion.
- **Erode** (E) — background hydraulic + thermal erosion with a live progress
  bar: slope-gated droplets, flow-accumulation-carved dendritic channels,
  smoothed sediment fans. Tune it under the panel's Erosion → Realism section.
- **Stamp** (T) — float a grayscale heightfield PNG under the cursor as a live
  GPU preview; wheel = strength (negative carves), Ctrl+wheel = size,
  Shift+wheel = rotate; click commits. Drop your own 8/16-bit PNGs into the
  gallery folder (`crates/bevy_wilderness_editor_ui/assets/stamps/`, generated
  on first run) and hit Rescan.
- **Undo/redo** — Ctrl+Z / Ctrl+Shift+Z, every tool and erosion included.
- **Export** — writes the heightmap as both `.ktx2` (engine master) and
  16-bit `.png` (interchange). Reload an export without restarting via the
  panel's "Load terrain…", or at launch:

```sh
> WILDERNESS_HEIGHTMAP=heightmap_export.ktx2 cargo run -p bevy_wilderness_editor_ui --example editor
```

`WILDERNESS_NEW=<texels>` picks the starting resolution of the default new
terrain (world footprint is unchanged).

## Height fog

Exponential height fog with two rendering tiers — `FogTier::High` (fullscreen
post-process, fogs the sky too) and `FogTier::Low` (inline, virtually free, for
standalone VR) — so one binary serves flatscreen and VR. Add `HeightFogPlugin`,
set the `TerrainFog` (look) and `TerrainQuality` (performance profile, `Low`/
`Medium`/`High`) resources, and use `HeightFogExtension` (or the
`bevy_wilderness::fog_functions` shader include + `InlineFog`) to fog your own meshes.
See [`examples/basic.rs`](crates/bevy_wilderness/examples/basic.rs).

## Compatible Bevy versions

| `bevy_wilderness` | `bevy`   |
| :--               | :--      |
| `0.1`             | `0.19.0` |

## Contributing

PRs are welcome. This is a terrain editor for my game Totem, so PR's need to be aligned with the games goals.
