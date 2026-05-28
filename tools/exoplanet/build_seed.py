#!/usr/bin/env python3
"""
Build the curated seed for the exoplanet_ndb demo.

Reads NASA Exoplanet Archive CSV ({.demo-data/source-data/nasa-exoplanets-default.csv})
and emits a JSON file ({exoplanet_explorer/seed.json}) with ~85 planets across
all 11 detection methods plus iconic multi-planet systems.

Curation strategy: tell the history of exoplanet hunting through 1992–2025.
- All firsts (pulsar 1992, RV 1995, transit 1999, microlensing 2005, imaging 2004)
- All 11 methods represented (rare ones too — astrometry, disk kinematics, OBM)
- High-arity systems (TRAPPIST-1=7, Kepler-90=8, HD 110067=6, TOI-178=6)
- Habitable-zone benchmarks
- Atmospheric study targets

Re-run after updating the WANT list:
    python3 tools/exoplanet/build_seed.py
"""
from __future__ import annotations

import csv
import json
import pathlib
import sys
from collections import OrderedDict

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
CSV_PATH = REPO_ROOT / ".demo-data" / "source-data" / "nasa-exoplanets-default.csv"
OUT_PATH = REPO_ROOT / "crates" / "ndb-renderer" / "examples" / "exoplanet_explorer" / "seed.json"

# Curated planets — verified against the NASA CSV (May 2026 snapshot).
# Names use the canonical pl_name from the archive. Comments explain why
# each is in the demo.
WANT = [
    # ── Pulsar timing — first exoplanets ever (1992–1994) ──
    "PSR B1257+12 b", "PSR B1257+12 c", "PSR B1257+12 d",
    # PSR B1620-26 b — circumbinary pulsar planet "Methuselah" (12.7 Gyr)
    "PSR B1620-26 b",

    # ── Radial Velocity pioneers ──
    "51 Peg b",                            # 1st around sun-like star (1995, Nobel 2019)
    "HD 209458 b",                         # 1st transit (1999), also RV-discovered first
    "HD 189733 b",                         # iconic atmosphere studies
    "HD 80606 b",                          # extreme eccentricity (0.93)
    "55 Cnc b", "55 Cnc c", "55 Cnc d", "55 Cnc e", "55 Cnc f",   # 5-planet RV system
    "HD 10180 c", "HD 10180 d", "HD 10180 e", "HD 10180 f", "HD 10180 g", "HD 10180 h",  # 6-planet RV
    "tau Cet f", "tau Cet g", "tau Cet h",  # nearby star, HZ candidates
    "Proxima Cen b", "Proxima Cen d",       # closest exoplanet

    # ── Transit — Kepler-era ──
    "Kepler-22 b",                         # 1st Kepler habitable-zone (2011)
    "Kepler-186 f",                        # 1st Earth-size HZ (2014)
    "Kepler-1649 c",                       # late Kepler HZ rediscovery (2020)
    "Kepler-19 c",                         # 1st TTV detection (registered as TTV below)
    "Kepler-51 b", "Kepler-51 c", "Kepler-51 d",  # super-puffs
    "GJ 1214 b",                           # iconic mini-Neptune / steam world
    "GJ 9827 b", "GJ 9827 c", "GJ 9827 d",
    "WASP-12 b",                           # carbon-rich hot Jupiter
    "WASP-121 b",                          # atmospheric escape
    "WASP-189 b",                          # ultra-hot UV
    "WASP-107 b",                          # low-density warm Saturn
    "KELT-9 b",                            # hottest known (4600 K)
    # TRAPPIST-1 — 7 rocky planets (2017)
    "TRAPPIST-1 b", "TRAPPIST-1 c", "TRAPPIST-1 d", "TRAPPIST-1 e",
    "TRAPPIST-1 f", "TRAPPIST-1 g", "TRAPPIST-1 h",
    # Kepler-90 / KOI-351 — 8 planets, tied for most
    "KOI-351 b", "KOI-351 c", "Kepler-90 i", "KOI-351 d",
    "KOI-351 e", "KOI-351 f", "KOI-351 g", "KOI-351 h",
    # TOI-178 — 6 planets in resonant chain
    "TOI-178 b", "TOI-178 c", "TOI-178 d", "TOI-178 e", "TOI-178 f", "TOI-178 g",
    # HD 110067 — 6 in resonance (2023)
    "HD 110067 b", "HD 110067 c", "HD 110067 d", "HD 110067 e", "HD 110067 f", "HD 110067 g",
    # TOI-700 — TESS HZ system
    "TOI-700 b", "TOI-700 c", "TOI-700 d", "TOI-700 e",
    "LHS 1140 b", "LHS 1140 c",
    "K2-18 b", "K2-18 c",                  # JWST atmosphere studies

    # ── Microlensing — first cold-rocky (2005) ──
    "OGLE-2005-BLG-390L b",

    # ── Direct Imaging — first (2004) + HR 8799 system ──
    "2MASS J12073346-3932539 b",           # 2M1207 b, first directly imaged planet
    "HR 8799 b", "HR 8799 c", "HR 8799 d", "HR 8799 e",

    # ── Astrometry — Gaia era ──
    "Gaia-4 b",                            # Gaia astrometric detection (2025)
    "HD 128717 b",                         # Gaia (2025)

    # ── Disk Kinematics — the only one ──
    "HD 97048 b",                          # ALMA disk kinematics (2019)

    # ── Pulsation Timing Variations ──
    "V0391 Peg b",                         # 2007, around subdwarf B star
    "KIC 7917485 b",                       # 2016

    # ── Orbital Brightness Modulation (BEER) ──
    "Kepler-76 b",                         # 2013, famous BEER detection
    "KOI-55 b", "KOI-55 c",                # 2011, around hot subdwarf

    # ── Eclipse Timing Variations ──
    "NN Ser c", "NN Ser d",                # 2010, around eclipsing binary
]

# Habitable-zone subset — kept here so the seeder can build an HZ
# hyperedge per system that has HZ planets. Verified rough heuristics
# from published papers (more nuanced than a single equilib-temp cutoff).
HZ_PLANETS = {
    "Kepler-22 b", "Kepler-186 f", "Kepler-1649 c",
    "TOI-700 d", "TOI-700 e",
    "TRAPPIST-1 d", "TRAPPIST-1 e", "TRAPPIST-1 f", "TRAPPIST-1 g",
    "LHS 1140 b",
    "Proxima Cen b",
    "K2-18 b",
    "tau Cet f", "tau Cet g",
}

# ── helpers ────────────────────────────────────────────────────────────

def f_or_none(s: str):
    if s is None or s == "":
        return None
    try:
        return float(s)
    except ValueError:
        return None


def i_or_none(s: str):
    if s is None or s == "":
        return None
    try:
        return int(s)
    except ValueError:
        return None


def normalise_method(m: str) -> str:
    """Compact display name — keep matching NASA's canonical method strings."""
    return m or "Unknown"


def normalise_mission(facility: str) -> str:
    """Pull a short label from disc_facility — many entries are verbose like
    'Transiting Exoplanet Survey Satellite (TESS)'. We keep the leading
    full name as the entity's PROP_FULL_NAME, but ID the entity by a
    short key for stable cross-system references."""
    f = (facility or "").strip()
    short_map = [
        ("Transiting Exoplanet Survey Satellite", "TESS"),
        ("Kepler", "Kepler"),
        ("K2", "K2"),
        ("CoRoT", "CoRoT"),
        ("Cheops", "CHEOPS"),
        ("Gaia", "Gaia"),
        ("James Webb", "JWST"),
        ("Hubble Space Telescope", "Hubble"),
        ("Spitzer", "Spitzer"),
        ("Atacama Large Millimeter", "ALMA"),
        ("Very Large Array", "VLA"),
        ("Very Long Baseline", "VLBA"),
        ("Keck", "Keck"),
        ("Gemini", "Gemini"),
        ("Subaru", "Subaru"),
        ("La Silla", "La Silla"),
        ("Paranal", "VLT"),
        ("MMT", "MMT"),
        ("OGLE", "OGLE"),
        ("MOA", "MOA"),
        ("HATNet", "HATNet"),
        ("WASP", "WASP"),
        ("KELT", "KELT"),
        ("HARPS", "HARPS"),
        ("HIRES", "HIRES"),
        ("Anglo-Australian", "AAT"),
        ("Multiple Observatories", "Multiple"),
        ("Multiple Facilities", "Multiple"),
        ("Haute-Provence", "OHP"),
        ("Arecibo", "Arecibo"),
        ("Parkes", "Parkes"),
        ("McDonald", "McDonald"),
        ("South African Radio", "SARAO"),
    ]
    for needle, short in short_map:
        if needle.lower() in f.lower():
            return short
    return f or "Unknown"


# ── build ──────────────────────────────────────────────────────────────

def main():
    if not CSV_PATH.exists():
        sys.exit(f"CSV not found at {CSV_PATH}. Run the fetch step first.")

    rows_by_name = {r["pl_name"]: r for r in csv.DictReader(open(CSV_PATH))}

    picks = []
    missing = []
    for name in WANT:
        r = rows_by_name.get(name)
        if not r:
            missing.append(name)
            continue
        picks.append(r)

    if missing:
        sys.exit(f"missing names: {missing}")

    # ── Stars (one per unique hostname) ─────────────────────────────
    stars = OrderedDict()
    for r in picks:
        host = r["hostname"]
        if host in stars:
            continue
        stars[host] = {
            "name": host,
            "ra":          f_or_none(r["ra"]),
            "dec":         f_or_none(r["dec"]),
            "distance_pc": f_or_none(r["sy_dist"]),
            "teff_k":      f_or_none(r["st_teff"]),
            "mass_msun":   f_or_none(r["st_mass"]),
            "radius_rsun": f_or_none(r["st_rad"]),
            "age_gyr":     f_or_none(r["st_age"]),
        }

    # ── Methods + missions (one per unique) ────────────────────────
    methods = OrderedDict()
    missions = OrderedDict()
    for r in picks:
        m = normalise_method(r["discoverymethod"])
        if m not in methods:
            methods[m] = {"name": m}
        miss = normalise_mission(r["disc_facility"])
        if miss not in missions:
            missions[miss] = {
                "short": miss,
                "full":  r["disc_facility"] or miss,
            }

    # ── Planets + discovery hyperedges ─────────────────────────────
    planets = []
    discoveries = []
    for r in picks:
        name = r["pl_name"]
        host = r["hostname"]
        method = normalise_method(r["discoverymethod"])
        mission = normalise_mission(r["disc_facility"])
        planets.append({
            "name":           name,
            "host":           host,
            "radius_re":      f_or_none(r["pl_rade"]),
            "mass_me":        f_or_none(r["pl_bmasse"]),
            "orbital_period": f_or_none(r["pl_orbper"]),
            "semi_major_au":  f_or_none(r["pl_orbsmax"]),
            "eq_temp_k":      f_or_none(r["pl_eqt"]),
            "habitable":      name in HZ_PLANETS,
        })
        discoveries.append({
            "planet":   name,
            "star":     host,
            "method":   method,
            "mission":  mission,
            "year":     i_or_none(r["disc_year"]),
            "facility": r["disc_facility"],
            "telescope": r["disc_telescope"],
        })

    # ── Systems hyperedges (one per host, ordered by orbital period) ──
    by_host = {}
    for p in planets:
        by_host.setdefault(p["host"], []).append(p)
    systems = []
    for host, ps in by_host.items():
        ps.sort(key=lambda p: p["orbital_period"] if p["orbital_period"] is not None else 1e18)
        systems.append({
            "host":    host,
            "planets": [p["name"] for p in ps],
            "n":       len(ps),
        })

    # ── HZ hyperedges (one per system that has HZ planets) ─────────
    hz_groups = []
    for s in systems:
        hz = [n for n in s["planets"] if n in HZ_PLANETS]
        if hz:
            hz_groups.append({"host": s["host"], "planets": hz})

    out = {
        "schema_note": "exoplanet_ndb seed v1. NASA Exoplanet Archive snapshot 2026-05-28. See tools/exoplanet/build_seed.py.",
        "counts": {
            "planets":      len(planets),
            "stars":        len(stars),
            "methods":      len(methods),
            "missions":     len(missions),
            "systems":      len(systems),
            "discoveries":  len(discoveries),
            "hz_groups":    len(hz_groups),
        },
        "stars":       list(stars.values()),
        "methods":     list(methods.values()),
        "missions":    list(missions.values()),
        "planets":     planets,
        "discoveries": discoveries,
        "systems":     systems,
        "hz_groups":   hz_groups,
    }

    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_text(json.dumps(out, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {OUT_PATH}")
    for k, v in out["counts"].items():
        print(f"  {k:13s} {v}")


if __name__ == "__main__":
    main()
