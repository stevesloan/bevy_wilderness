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

The example usage can be seen in the [examples](examples/basic.rs) directory.
This example uses a very low-resolution heightmap to save space when cloning this repository. For better visual results, create your own higher-resolution textures.

The example's terrain material uses CC0 texture sets that are not committed to the
repo. Fetch them once before running it:

```sh
> pip install Pillow
> python assets/fetch_textures.py   # downloads grass/dirt/rock/snow into assets/terrain/
> cargo run --example basic
```

To use your own textures, load one image file per layer with `load_terrain_array`
(albedo `srgb=true`; normal and ORM `srgb=false`, where ORM packs occlusion,
roughness, metallic into R, G, B). See `examples/basic.rs`.

Normal maps must be **OpenGL convention** (+Y / green points up), like Poly Haven's
`nor_gl` set. If crevices and cracks look like raised bumps or veins, your normals
are DirectX convention — invert the green channel to convert them.

## Height fog

Exponential height fog with two rendering tiers — `FogTier::High` (fullscreen
post-process, fogs the sky too) and `FogTier::Low` (inline, virtually free, for
standalone VR) — so one binary serves flatscreen and VR. Add `HeightFogPlugin`,
set the `TerrainFog` (look) and `TerrainQuality` (performance profile, `Low`/
`Medium`/`High`) resources, and use `HeightFogExtension` (or the
`bevy_wilderness::fog_functions` shader include + `InlineFog`) to fog your own meshes.
See [`examples/basic.rs`](examples/basic.rs).

## How to create textures

To create heightmap textures you can use the [clipmap.py](convert/clipmap.py) script.

First of all, you have to install required libraries:
```sh
> pip install -r requirements.txt
```

```sh
> python clipmap.py --help
usage: clipmap.py [-h] filename {ktx} ...

Heightmap processing tool for the bevy_wilderness plugin

positional arguments:
  filename       16-bit PNG heightmap
  {ktx}
    ktx          Convert the heightmap to KTX2

options:
  -h, --help     show this help message and exit
```

### Example usage:
```sh
> python clipmap.py heightmap.png ktx 8192 8192 # Convert 16-bit PNG to 8192x8192 KTX
```

## Compatible Bevy versions

| `bevy_wilderness` | `bevy`   |
| :--               | :--      |
| `0.1`             | `0.19.0` |

## Contributing

PRs are very welcome!
