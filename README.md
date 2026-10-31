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
- **Height fog**, one inline path (flatscreen + standalone VR).
- **Toroidal looping** — optionally tile the heightmap for seamless infinite terrain.
- **Editable-terrain API** (`editing` feature) — a `RebakeRequested` re-bake
  trigger and the CPU `Heightfield` query, the hooks the terrain editor builds on.

## Documentation

- [docs/architecture.md](docs/architecture.md) — how the renderer works.
- [docs/editor.md](docs/editor.md) — how the terrain editor works.
- [docs/roadmap.md](docs/roadmap.md) — where it's going, and why.

## Usage

The example's terrain material uses CC0 texture sets that are not committed to the
repo. Fetch them once before running it:

```sh
> pip install Pillow
> python crates/bevy_wilderness/assets/fetch_textures.py   # downloads grass/dirt/rock/sediment into crates/bevy_wilderness/assets/terrain/
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
egui side panel driving that same public API). See
[docs/editor.md](docs/editor.md).

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

- **Sculpt** (S) — raise / lower / smooth / flatten under an adjustable brush;
  hold Shift to smooth or Ctrl to invert without leaving the current mode.
- **Mask** (M) — paint a feathered mask that confines sculpting and stamps.
- **Stamp** (T) — float a grayscale heightfield PNG under the cursor as a live
  GPU preview; wheel = strength (negative carves), Ctrl+wheel = size,
  Shift+wheel = rotate; click commits. Drop your own 8/16-bit PNGs into the
  gallery folder (`crates/bevy_wilderness_editor_ui/assets/stamps/`, generated
  on first run) and hit Rescan.
- **Undo/redo** — Ctrl+Z / Ctrl+Shift+Z, every tool included.
- **Export** — writes the heightmap as both `.ktx2` (engine master) and
  16-bit `.png` (interchange). Reload an export without restarting via the
  panel's "Load terrain…", or at launch:

```sh
> WILDERNESS_HEIGHTMAP=heightmap_export.ktx2 cargo run -p bevy_wilderness_editor_ui --example editor
```

`WILDERNESS_NEW=<texels>` picks the starting resolution of the default new
terrain (world footprint is unchanged).

## Height fog

Analytic exponential height fog, per pixel in the terrain's shader, with no pass of
its own, so it's the same on flatscreen and standalone VR and leaves MSAA free. Set
the `TerrainFog` resource for its look. To fog your own materials, import
`bevy_wilderness::fog_functions`, keep a `HeightFogParams` uniform on `InlineFog`, and
call `apply_height_fog` before tonemapping. Fog colour is in cd/m², so the camera's
exposure applies to it like any other light.

## Compatible Bevy versions

| `bevy_wilderness` | `bevy`   |
| :--               | :--      |
| `0.1`             | `0.20.0` |

## Contributing

PRs are welcome. This is a terrain editor for my game Totem, so PR's need to be aligned with the games goals.
