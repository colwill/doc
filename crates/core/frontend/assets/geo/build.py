#!/usr/bin/env python3
"""Writes countries-110m.json, the world `doc-geomap.js` draws, from Natural Earth's 1:110m
countries (public domain, https://www.naturalearthdata.com): each country's codes, names, label
point and outline rounded to a hundredth of a degree, which is finer than the source's own scale.

Natural Earth publishes 838 KB, nearly all of it names in other languages. The release is pinned
and its hash checked, so running this again writes the same file; change both to move release.
"""

import hashlib
import json
import pathlib
import urllib.request

SOURCE = (
    "https://raw.githubusercontent.com/nvkelso/natural-earth-vector/v5.1.2/"
    "geojson/ne_110m_admin_0_countries.geojson"
)
SHA256 = "6866c877d39cba9c357620878839b336d569f8c662d3cfab4cb1dbe2d39c977f"
OUT = pathlib.Path(__file__).with_name("countries-110m.json")


def code(properties, *keys):
    """The first of `keys` that holds a real code: Natural Earth writes -99 where ISO has none."""
    for key in keys:
        value = properties.get(key) or ""
        if value and value != "-99":
            return value
    return ""


def rings(geometry):
    polygons = geometry["coordinates"]
    if geometry["type"] == "Polygon":
        polygons = [polygons]
    found = []
    for polygon in polygons:
        for ring in polygon:
            flat = []
            for lon, lat in ring:
                flat.extend((round(lon, 2), round(lat, 2)))
            found.append(flat)
    return found


def main():
    raw = urllib.request.urlopen(SOURCE, timeout=60).read()
    digest = hashlib.sha256(raw).hexdigest()
    if digest != SHA256:
        raise SystemExit(f"Natural Earth's file has changed: {digest}, expected {SHA256}")
    countries = []
    for feature in json.loads(raw)["features"]:
        properties = feature["properties"]
        names = []
        for key in ("NAME", "NAME_LONG", "ADMIN", "FORMAL_EN"):
            name = (properties.get(key) or "").strip()
            if name and name not in names:
                names.append(name)
        countries.append({
            "a2": code(properties, "ISO_A2", "ISO_A2_EH"),
            "a3": code(properties, "ISO_A3", "ISO_A3_EH", "ADM0_A3"),
            "names": names,
            "label": [round(properties["LABEL_X"], 2), round(properties["LABEL_Y"], 2)],
            "rings": rings(feature["geometry"]),
        })
    countries.sort(key=lambda country: country["names"][0])
    world = {
        "source": "Natural Earth 1:110m Admin 0 countries, v5.1.2. Public domain.",
        "countries": countries,
    }
    OUT.write_text(json.dumps(world, separators=(",", ":"), ensure_ascii=False) + "\n")
    print(f"wrote {OUT.name}: {len(countries)} countries, {OUT.stat().st_size} bytes")


if __name__ == "__main__":
    main()
