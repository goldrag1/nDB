#!/usr/bin/env python3
"""
Pull curated historic mega-quake aftershock windows from the USGS FDSN
event API.

USGS reliably has machine-readable catalogues back to ~1973 (ANSS Comcat),
plus inserted historic strong-motion events earlier. For each curated
mainshock we query the API for events in a ~30-day window after it,
within a bounding box around the epicentre, with magnitude ≥ 4.5 (cuts
the catalog from millions of small detections to a few hundred meaningful
aftershocks per sequence).

Output: one GeoJSON file per mega-quake under
.demo-data/source-data/seismic/historic/<slug>.geojson — used by
tools/seismic/build_seed.py.

Re-run after editing MEGA_QUAKES (idempotent — overwrites cached files).
"""
from __future__ import annotations

import json
import pathlib
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT_DIR = REPO_ROOT / ".demo-data" / "source-data" / "seismic" / "historic"

# Curated mega-quakes: name, slug, approximate epicentre lat/lon, start of
# window, and a ±delta latitude/longitude span generous enough to capture
# the aftershock cluster but tight enough to exclude unrelated events.
# Order matters in the SPA dropdown — we present chronologically.
MEGA_QUAKES = [
    # slug,            name,                             lat,    lon,     starttime,    window_days, lat_span, lon_span, min_mag
    ("1989-loma-prieta", "1989 Loma Prieta (California, M6.9)",   37.04, -121.88, "1989-10-17", 60, 1.5, 1.5, 4.0),
    ("1995-kobe",        "1995 Kobe (Japan, M6.9)",               34.59, 135.07,  "1995-01-17", 60, 1.5, 1.5, 4.0),
    ("2004-sumatra",     "2004 Sumatra-Andaman (M9.1)",            3.32, 95.85,   "2004-12-26", 90, 8.0, 8.0, 5.0),
    ("2008-sichuan",     "2008 Sichuan (China, M7.9)",            31.00, 103.32,  "2008-05-12", 60, 2.5, 3.0, 4.5),
    ("2010-haiti",       "2010 Haiti (M7.0)",                     18.46, -72.53,  "2010-01-12", 60, 2.0, 2.5, 4.0),
    ("2010-maule",       "2010 Maule (Chile, M8.8)",             -36.12, -72.90,  "2010-02-27", 90, 6.0, 5.0, 4.5),
    ("2011-tohoku",      "2011 Tōhoku (Japan, M9.1)",             38.30, 142.37,  "2011-03-11", 60, 5.0, 5.0, 5.0),
    ("2015-nepal",       "2015 Gorkha (Nepal, M7.8)",             28.23, 84.73,   "2015-04-25", 60, 2.5, 3.0, 4.5),
    ("2017-puebla",      "2017 Puebla (Mexico, M7.1)",            18.55, -98.49,  "2017-09-19", 30, 2.0, 2.5, 4.0),
    ("2018-anchorage",   "2018 Anchorage (Alaska, M7.1)",         61.35, -149.93, "2018-11-30", 60, 3.0, 4.0, 4.0),
    ("2019-ridgecrest",  "2019 Ridgecrest (California, M7.1)",    35.77, -117.60, "2019-07-06", 30, 1.5, 1.5, 4.0),
    ("2023-turkey",      "2023 Türkiye-Syria (M7.8 + M7.5)",      37.17, 37.03,   "2023-02-06", 60, 3.0, 4.0, 4.5),
    ("2025-myanmar",     "2025 Myanmar Sagaing fault (M7.7)",     21.93, 95.99,   "2025-03-28", 60, 3.0, 3.0, 4.5),
]


def days_to_endtime(start: str, days: int) -> str:
    import datetime as dt
    sd = dt.datetime.strptime(start, "%Y-%m-%d")
    return (sd + dt.timedelta(days=days)).strftime("%Y-%m-%d")


def fetch_one(slug, name, lat, lon, starttime, window_days, lat_span, lon_span, min_mag):
    params = {
        "format":       "geojson",
        "starttime":    starttime,
        "endtime":      days_to_endtime(starttime, window_days),
        "minlatitude":  lat - lat_span,
        "maxlatitude":  lat + lat_span,
        "minlongitude": lon - lon_span,
        "maxlongitude": lon + lon_span,
        "minmagnitude": min_mag,
        "orderby":      "magnitude",
        "limit":        1000,
    }
    url = "https://earthquake.usgs.gov/fdsnws/event/1/query?" + urllib.parse.urlencode(params)
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    out = OUT_DIR / f"{slug}.geojson"
    print(f"[{slug}] {name}", flush=True)
    print(f"  → {url}", flush=True)
    try:
        req = urllib.request.Request(url, headers={"User-Agent": "nDB seismic_ndb demo seed builder (+github.com/goldrag1/nDB)"})
        with urllib.request.urlopen(req, timeout=30) as r:
            data = r.read()
    except urllib.error.HTTPError as e:
        print(f"  HTTP {e.code} {e.reason}", flush=True)
        return
    except Exception as e:  # noqa: BLE001
        print(f"  ERROR {e}", flush=True)
        return
    out.write_bytes(data)
    try:
        d = json.loads(data)
        n = len(d.get("features", []))
        mags = [f["properties"]["mag"] for f in d["features"] if f["properties"].get("mag") is not None]
        print(f"  ✓ {n} events  (mag range {min(mags):.1f}–{max(mags):.1f})" if mags else f"  ✓ {n} events", flush=True)
    except Exception:
        print(f"  ✓ wrote {len(data)} bytes (parse failed)", flush=True)
    # USGS recommends polite pause between large queries.
    time.sleep(0.5)


def main():
    for row in MEGA_QUAKES:
        fetch_one(*row)
    print(f"\nDone. {len(MEGA_QUAKES)} mega-quakes cached under {OUT_DIR}")


if __name__ == "__main__":
    main()
