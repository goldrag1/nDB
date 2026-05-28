#!/usr/bin/env python3
"""
Build the curated seed for the seismic_ndb demo.

Reads:
- .demo-data/source-data/seismic/all_month.geojson  (USGS 30-day feed)
- .demo-data/source-data/seismic/historic/*.geojson (curated mega-quakes)

Emits:
- crates/ndb-renderer/examples/seismic_explorer/seed.json

Pipeline:
1. Live feed: keep events with M ≥ 2.5 (cuts ~10.6k → ~1.8k while
   preserving every significant event of the month).
2. Detect aftershock sequences in the live feed: for each M ≥ 6.0
   event, group lower-magnitude events within 30 days + 200 km as
   its aftershocks. Form one AFTERSHOCK_SEQUENCE hyperedge per.
3. Historic mega-quakes: the largest event in each file is the
   mainshock; everything else with M ≥ 4.0 + smaller than the
   mainshock is an aftershock. One AFTERSHOCK_SEQUENCE per file.
4. Faults: a hand-curated list of ~12 major fault systems with
   approximate trace points. Each event within ~30 km of a fault
   trace gets a binary ON_FAULT hyperedge.
5. Agencies: one entity per `net` in the USGS feed (us, ci, nc, etc.).

Re-run idempotently after re-fetching:
    python3 tools/seismic/build_seed.py
"""
from __future__ import annotations

import json
import math
import pathlib
from collections import OrderedDict

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
SRC_DIR  = REPO_ROOT / ".demo-data" / "source-data" / "seismic"
LIVE_F   = SRC_DIR / "all_month.geojson"
HIST_DIR = SRC_DIR / "historic"
OUT_PATH = REPO_ROOT / "crates" / "ndb-renderer" / "examples" / "seismic_explorer" / "seed.json"

LIVE_MIN_MAG    = 2.5
SEQ_MAINSHOCK_M = 6.0     # live-feed mainshock threshold
SEQ_WINDOW_DAYS = 30
SEQ_RADIUS_KM   = 200
FAULT_LINK_KM   = 30      # an event ≤ this far from a fault trace → ON_FAULT


# ── Hand-curated major fault systems ─────────────────────────────────
# Coarse line traces for global recognisability. Each trace is a list
# of (lat, lon) points; events near any segment of any trace will be
# linked. Sources: GEM Global Active Faults DB + Wikipedia overviews.
FAULTS = [
    {"name": "San Andreas Fault",         "type": "right-lateral strike-slip",
     "country": "USA",
     "trace": [(40.45, -124.40), (39.50, -123.80), (38.10, -122.86), (37.40, -122.16),
               (36.45, -121.10), (35.75, -120.30), (34.85, -118.93), (33.40, -116.10)]},
    {"name": "Hayward Fault",             "type": "right-lateral strike-slip",
     "country": "USA",
     "trace": [(38.07, -122.27), (37.81, -122.20), (37.55, -121.92), (37.30, -121.78)]},
    {"name": "Cascadia Subduction Zone",  "type": "subduction megathrust",
     "country": "USA/Canada",
     "trace": [(48.50, -127.50), (47.00, -126.50), (45.50, -125.50), (43.50, -125.10), (41.00, -124.80)]},
    {"name": "Sumatran Fault",            "type": "right-lateral strike-slip",
     "country": "Indonesia",
     "trace": [(5.50, 95.30), (4.20, 96.40), (2.40, 98.40), (0.50, 100.10), (-2.00, 101.40), (-4.50, 103.20), (-6.10, 105.20)]},
    {"name": "Sunda Subduction Zone",     "type": "subduction megathrust",
     "country": "Indonesia",
     "trace": [(7.50, 92.50), (4.00, 94.50), (0.00, 97.00), (-4.00, 100.50), (-8.00, 106.00), (-10.00, 112.00), (-10.50, 119.00)]},
    {"name": "Japan Trench",              "type": "subduction megathrust",
     "country": "Japan",
     "trace": [(43.50, 145.00), (41.00, 144.00), (38.50, 144.00), (36.00, 142.30), (34.00, 141.50)]},
    {"name": "Median Tectonic Line",      "type": "right-lateral strike-slip",
     "country": "Japan",
     "trace": [(35.60, 138.30), (35.20, 137.10), (34.60, 135.40), (34.10, 133.10), (33.70, 131.00)]},
    {"name": "North Anatolian Fault",     "type": "right-lateral strike-slip",
     "country": "Türkiye",
     "trace": [(40.92, 28.30), (40.78, 30.10), (40.85, 32.10), (40.71, 34.30), (40.45, 36.50), (39.95, 39.20), (39.45, 41.00)]},
    {"name": "East Anatolian Fault",      "type": "left-lateral strike-slip",
     "country": "Türkiye",
     "trace": [(38.50, 39.40), (37.70, 38.80), (37.10, 37.30), (36.40, 36.10), (35.85, 35.85)]},
    {"name": "Dead Sea Transform",        "type": "left-lateral strike-slip",
     "country": "Israel/Jordan/Syria",
     "trace": [(35.20, 36.15), (34.30, 36.20), (33.30, 35.95), (32.30, 35.55), (31.20, 35.40), (29.50, 34.95)]},
    {"name": "Alpine Fault",              "type": "right-lateral strike-slip",
     "country": "New Zealand",
     "trace": [(-40.50, 172.60), (-42.00, 171.70), (-43.50, 170.50), (-44.20, 168.80)]},
    {"name": "Sagaing Fault",             "type": "right-lateral strike-slip",
     "country": "Myanmar",
     "trace": [(26.50, 96.00), (24.50, 96.10), (22.50, 96.00), (20.50, 96.10), (18.30, 96.50), (16.50, 96.20)]},
    {"name": "Main Himalayan Thrust",     "type": "continental collision thrust",
     "country": "Nepal/India",
     "trace": [(34.50, 75.00), (32.50, 78.00), (30.50, 80.50), (28.50, 84.00), (27.00, 87.00), (26.40, 89.50), (27.10, 92.50), (28.50, 96.00)]},
    {"name": "East African Rift (East)",  "type": "continental rift",
     "country": "Kenya/Ethiopia",
     "trace": [(12.50, 41.50), (9.50, 40.00), (5.00, 37.50), (1.50, 36.50), (-2.50, 36.30), (-6.00, 36.00), (-9.50, 34.50), (-13.00, 32.50)]},
]


# ── Helpers ──────────────────────────────────────────────────────────

def haversine_km(lat1, lon1, lat2, lon2):
    """Great-circle distance in km."""
    R = 6371.0
    p1 = math.radians(lat1); p2 = math.radians(lat2)
    dp = math.radians(lat2 - lat1); dl = math.radians(lon2 - lon1)
    a = math.sin(dp / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(dl / 2) ** 2
    return 2 * R * math.asin(math.sqrt(a))


def distance_to_polyline_km(lat, lon, trace):
    """Min great-circle distance from (lat,lon) to any vertex of `trace`.
    Approximation — true point-to-segment on a sphere is heavier; for
    fault linkage with a 30 km threshold the vertex distance is close
    enough since traces are densely sampled."""
    return min(haversine_km(lat, lon, plat, plon) for plat, plon in trace)


def feature_to_event(feature, source_tag):
    """Normalise a USGS GeoJSON feature into our seed dict."""
    p = feature.get("properties", {}) or {}
    geom = feature.get("geometry", {}) or {}
    coords = geom.get("coordinates") or [None, None, None]
    lon, lat, depth = coords[0], coords[1], coords[2] if len(coords) > 2 else None
    if lat is None or lon is None:
        return None
    return {
        "id":          feature.get("id"),
        "mag":         p.get("mag"),
        "mag_type":    p.get("magType"),
        "place":       p.get("place"),
        "time_ms":     p.get("time"),       # unix epoch ms
        "tsunami":     bool(p.get("tsunami")),
        "felt":        p.get("felt"),
        "sig":         p.get("sig"),
        "net":         p.get("net"),
        "alert":       p.get("alert"),
        "lat":         lat,
        "lon":         lon,
        "depth_km":    depth,
        "source":      source_tag,         # "live30d" or historic slug
    }


# ── Aftershock sequence detection ────────────────────────────────────

def detect_sequences_in_live(events):
    """For each M≥6.0 event in `events`, find lower-magnitude later events
    within SEQ_WINDOW_DAYS / SEQ_RADIUS_KM. Returns a list of dicts:
        { "mainshock_id": ..., "aftershock_ids": [...] }
    """
    by_t = sorted([e for e in events if e["mag"] is not None and e["time_ms"] is not None],
                   key=lambda e: e["time_ms"])
    seqs = []
    window_ms = SEQ_WINDOW_DAYS * 86400 * 1000
    for ms in by_t:
        if (ms["mag"] or 0) < SEQ_MAINSHOCK_M:
            continue
        t0 = ms["time_ms"]
        afters = []
        for ev in by_t:
            if ev["id"] == ms["id"]:
                continue
            if ev["time_ms"] <= t0 or ev["time_ms"] > t0 + window_ms:
                continue
            if (ev["mag"] or 0) >= ms["mag"]:
                continue
            d = haversine_km(ms["lat"], ms["lon"], ev["lat"], ev["lon"])
            if d > SEQ_RADIUS_KM:
                continue
            afters.append(ev["id"])
        if afters:
            seqs.append({
                "mainshock_id":   ms["id"],
                "aftershock_ids": afters,
                "name":           f"Live: {ms['place']} (M{ms['mag']:.1f})" if ms.get("place") else ms["id"],
                "window_days":    SEQ_WINDOW_DAYS,
                "radius_km":      SEQ_RADIUS_KM,
            })
    return seqs


def detect_historic_sequence(events, label):
    """The single mainshock in a historic file is the largest event.
    Everything else strictly smaller in magnitude becomes an aftershock."""
    valid = [e for e in events if e["mag"] is not None]
    if not valid:
        return None
    valid.sort(key=lambda e: e["mag"], reverse=True)
    main = valid[0]
    afters = [e["id"] for e in valid[1:] if (e["mag"] or 0) < main["mag"]]
    return {
        "mainshock_id":   main["id"],
        "aftershock_ids": afters,
        "name":           label,
        "window_days":    SEQ_WINDOW_DAYS,
        "radius_km":      SEQ_RADIUS_KM,
        "historic":       True,
    }


# ── Build ────────────────────────────────────────────────────────────

def main():
    if not LIVE_F.exists():
        raise SystemExit(f"missing {LIVE_F} — run fetch_historic.py and/or curl the all_month feed first")

    # 1. Live 30-day feed.
    live_data = json.load(open(LIVE_F))
    live_events = []
    for feat in live_data["features"]:
        mag = (feat.get("properties") or {}).get("mag")
        if mag is None or mag < LIVE_MIN_MAG:
            continue
        e = feature_to_event(feat, "live30d")
        if e: live_events.append(e)

    # 2. Historic — one sequence per file, plus the events themselves.
    historic_events_by_id = {}
    historic_sequences = []
    historic_label_by_slug = {}
    if HIST_DIR.exists():
        # Pull display label from the slug → human name map embedded in fetch_historic.py.
        # We don't import that module; instead, the slug is human-ish (e.g. "2011-tohoku"),
        # so we synthesise a label from the slug.
        for path in sorted(HIST_DIR.glob("*.geojson")):
            slug = path.stem
            hd = json.load(open(path))
            evs = []
            for feat in hd.get("features", []):
                e = feature_to_event(feat, f"historic:{slug}")
                if e and e["mag"] is not None:
                    evs.append(e)
            if not evs:
                continue
            for e in evs:
                # Deduplicate by USGS event id — historic windows can
                # overlap (e.g. close mega-quakes share aftershocks).
                if e["id"] not in historic_events_by_id:
                    historic_events_by_id[e["id"]] = e
            label = slug.replace("-", " ").title()
            historic_label_by_slug[slug] = label
            seq = detect_historic_sequence(evs, label)
            if seq:
                historic_sequences.append(seq)

    # 3. Live sequences.
    live_sequences = detect_sequences_in_live(live_events)

    # Merge event sets — live takes precedence on id collision.
    all_events = list(historic_events_by_id.values())
    live_ids = set()
    for e in live_events:
        live_ids.add(e["id"])
    for e in live_events:
        if e["id"] not in historic_events_by_id:
            all_events.append(e)
    # Otherwise live overrides the historic copy.
    by_id = {e["id"]: e for e in all_events}
    for e in live_events:
        by_id[e["id"]] = e
    all_events = list(by_id.values())

    # 4. Agencies — one entity per `net`.
    agencies = OrderedDict()
    for e in all_events:
        n = e.get("net")
        if n and n not in agencies:
            agencies[n] = {"short": n.upper(), "code": n}

    # 5. Faults — emit entities + link events.
    on_fault = []   # list of {event_id, fault_name, distance_km}
    fault_index = {f["name"]: i for i, f in enumerate(FAULTS)}
    for e in all_events:
        best_name, best_d = None, FAULT_LINK_KM + 1
        for f in FAULTS:
            d = distance_to_polyline_km(e["lat"], e["lon"], f["trace"])
            if d < best_d:
                best_d, best_name = d, f["name"]
        if best_name is not None:
            on_fault.append({"event_id": e["id"], "fault_name": best_name, "distance_km": best_d})

    # 6. Emit.
    out = {
        "schema_note": "seismic_ndb seed v1. USGS Earthquake Catalog snapshot. See tools/seismic/build_seed.py.",
        "thresholds": {
            "live_min_magnitude":    LIVE_MIN_MAG,
            "sequence_mainshock_m":  SEQ_MAINSHOCK_M,
            "sequence_window_days":  SEQ_WINDOW_DAYS,
            "sequence_radius_km":    SEQ_RADIUS_KM,
            "fault_link_km":         FAULT_LINK_KM,
        },
        "counts": {
            "events":               len(all_events),
            "agencies":             len(agencies),
            "faults":               len(FAULTS),
            "live_sequences":       len(live_sequences),
            "historic_sequences":   len(historic_sequences),
            "on_fault_links":       len(on_fault),
        },
        "events":              all_events,
        "agencies":            list(agencies.values()),
        "faults":              FAULTS,
        "live_sequences":      live_sequences,
        "historic_sequences":  historic_sequences,
        "on_fault":            on_fault,
        "historic_labels":     historic_label_by_slug,
    }

    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_text(json.dumps(out, indent=1, default=str) + "\n", encoding="utf-8")
    print(f"wrote {OUT_PATH}  ({OUT_PATH.stat().st_size // 1024} KB)")
    for k, v in out["counts"].items():
        print(f"  {k:25s} {v}")


if __name__ == "__main__":
    main()
