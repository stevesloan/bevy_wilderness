#!/usr/bin/env python3
"""Fetch the CC0 terrain texture sets the basic example uses.

Downloads four Poly Haven (polyhaven.com, CC0) texture sets at 2K, converts
them to 8-bit, and packs occlusion + roughness into ORM maps, producing
`assets/terrain/{grass,dirt,rock,snow}_{albedo,normal,orm}.png`.

Requires Pillow: pip install Pillow
"""

import io
import json
import os
import urllib.request

from PIL import Image

# layer name -> Poly Haven asset slug
ASSETS = {
    "grass": "rocky_terrain_02",
    "dirt": "coast_land_rocks_01",
    "rock": "marble_cliff_05",
    "snow": "snow_field_aerial",
}
RESOLUTION = "2k"
# Poly Haven map key -> our suffix
MAPS = {"Diffuse": "albedo", "nor_gl": "normal", "Rough": "rough", "AO": "ao"}
UA = {"User-Agent": "Mozilla/5.0 bevy-clipmap-fetch"}

out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "terrain")
os.makedirs(out, exist_ok=True)


def get(url: str) -> bytes:
    return urllib.request.urlopen(urllib.request.Request(url, headers=UA)).read()


for layer, slug in ASSETS.items():
    files = json.loads(get(f"https://api.polyhaven.com/files/{slug}"))
    maps = {}
    for key, tag in MAPS.items():
        res = files[key][RESOLUTION]
        fmt = "png" if "png" in res else next(iter(res))
        print(f"{layer}_{tag} <- {res[fmt]['url']}")
        maps[tag] = Image.open(io.BytesIO(get(res[fmt]["url"])))

    # 8-bit RGB color + normal; ORM packs R=AO, G=roughness, B=metallic (0).
    # icc_profile=None: don't carry source profiles (AO's grayscale profile
    # would be wrong on the packed RGB ORM).
    maps["albedo"].convert("RGB").save(f"{out}/{layer}_albedo.png", icc_profile=None)
    maps["normal"].convert("RGB").save(f"{out}/{layer}_normal.png", icc_profile=None)
    ao = maps["ao"].convert("L")
    rough = maps["rough"].convert("L")
    orm = Image.merge("RGB", (ao, rough, Image.new("L", ao.size, 0)))
    orm.save(f"{out}/{layer}_orm.png", icc_profile=None)

print(f"done -> {out}")
