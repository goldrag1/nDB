#!/usr/bin/env python3
"""
Build seed.json for the biodiv_ndb demo.

For every species in interactions.SPECIES, hit the Wikipedia API to
fetch a representative photo URL (page-image thumbnail). Cached to
.photo-cache.json so the second run is offline.

Output:  crates/ndb-renderer/examples/biodiv_explorer/seed.json
"""
from __future__ import annotations
import json
import pathlib
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

# Mute pylint — same-package import that we just appended to sys.path.
from interactions import (  # type: ignore
    REGIONS, SPECIES, POLLINATION, MUTUALISM, PARASITISM, PREDATION, FOOD_WEBS,
)

OUT_PATH = (HERE.parent.parent
            / "crates" / "ndb-renderer" / "examples" / "biodiv_explorer"
            / "seed.json")

CACHE_PATH = HERE / ".photo-cache.json"

UA = "nDB-biodiv-demo/1.0 (https://ndb.nextstar-erp.com; nguyenhoanglong1@gmail.com)"
# Width 600 keeps each thumbnail ~30–80 KB; we render at ~150–200 px on screen,
# so 2× DPR is covered.
THUMB_WIDTH = 600

# ─── Wikipedia photo resolver ────────────────────────────────────────────

def _load_cache() -> dict:
    if CACHE_PATH.exists():
        try:
            return json.loads(CACHE_PATH.read_text())
        except json.JSONDecodeError:
            pass
    return {}


def _save_cache(cache: dict) -> None:
    CACHE_PATH.write_text(json.dumps(cache, indent=2, sort_keys=True))


def _api_get(url: str) -> dict:
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.loads(r.read())


def resolve_photo(binomial: str, cache: dict) -> dict | None:
    """Return {"photo_url": str, "wiki_url": str} for a binomial, or None."""
    if binomial in cache:
        c = cache[binomial]
        # honour negative-cache entries — re-tries are wasteful if a name simply isn't on en.wikipedia
        if c is None:
            return None
        return c

    titles = [binomial, binomial.split()[0]]  # try genus alone as fallback

    for title in titles:
        try:
            # 1. Resolve canonical title + page id + thumbnail (pageimage)
            qs = urllib.parse.urlencode({
                "action": "query",
                "titles": title,
                "prop": "pageimages|info",
                "piprop": "thumbnail|name|original",
                "pithumbsize": str(THUMB_WIDTH),
                "inprop": "url",
                "redirects": "1",
                "format": "json",
            })
            data = _api_get(f"https://en.wikipedia.org/w/api.php?{qs}")
            pages = (data.get("query") or {}).get("pages") or {}
            for _, p in pages.items():
                if "missing" in p:
                    continue
                thumb = (p.get("thumbnail") or {}).get("source")
                page_url = p.get("fullurl")
                if thumb:
                    result = {"photo_url": thumb, "wiki_url": page_url or ""}
                    cache[binomial] = result
                    _save_cache(cache)
                    return result
        except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError) as e:
            print(f"  ! {title}: {e}", file=sys.stderr)
        time.sleep(0.1)  # polite gap between attempts

    print(f"  ⊗ no photo for {binomial}", file=sys.stderr)
    cache[binomial] = None
    _save_cache(cache)
    return None


# ─── Seed assembly ───────────────────────────────────────────────────────

def build() -> dict:
    cache = _load_cache()
    seed: dict = {
        "regions": [],
        "species": [],
        "pollination":  [],
        "mutualism":    [],
        "parasitism":   [],
        "predation":    [],
        "food_webs":    [],
    }

    seed["regions"] = REGIONS

    # Species — resolve photos (with cache + rate limit)
    print(f"resolving {len(SPECIES)} species photos via Wikipedia ...")
    seen = set()
    for i, sp in enumerate(SPECIES):
        sci = sp["sci"]
        if sci in seen:
            print(f"  ! duplicate species in source: {sci}", file=sys.stderr)
            continue
        seen.add(sci)
        photo = resolve_photo(sci, cache) or {}
        seed["species"].append({
            **sp,
            "photo_url": photo.get("photo_url", ""),
            "wiki_url":  photo.get("wiki_url",  ""),
        })
        # Live progress
        if (i + 1) % 10 == 0:
            print(f"  ... {i + 1}/{len(SPECIES)}")
        # Polite gap between distinct species (cache hits skip the API entirely)
        if sci not in cache:
            time.sleep(0.25)

    # Interactions ─ flatten into the seed shape.
    for plant, poll, region, sf, st, oblig, note in POLLINATION:
        seed["pollination"].append({
            "plant": plant, "pollinator": poll, "region": region,
            "season_from": sf, "season_to": st, "obligate": oblig, "note": note,
        })
    for a, b, region, subtype, oblig, note in MUTUALISM:
        seed["mutualism"].append({
            "species_a": a, "species_b": b, "region": region,
            "subtype": subtype, "obligate": oblig, "note": note,
        })
    for host, parasite, region, mode, note in PARASITISM:
        seed["parasitism"].append({
            "host": host, "parasite": parasite, "region": region,
            "transmission": mode, "note": note,
        })
    for pred, prey, region, sf, st, note in PREDATION:
        seed["predation"].append({
            "predator": pred, "prey": prey, "region": region,
            "season_from": sf, "season_to": st, "note": note,
        })
    for fw in FOOD_WEBS:
        # Preserve trophic_edges (list of [predator, prey] sci-name pairs).
        # Default to [] for back-compat with any food-web entry missing it.
        seed["food_webs"].append({
            **fw,
            "trophic_edges": fw.get("trophic_edges", []),
        })

    return seed


def main() -> int:
    seed = build()
    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_text(json.dumps(seed, indent=2, ensure_ascii=False))

    n_species = len(seed["species"])
    n_with_photo = sum(1 for s in seed["species"] if s["photo_url"])
    n_inter = (
        len(seed["pollination"]) + len(seed["mutualism"])
        + len(seed["parasitism"]) + len(seed["predation"])
    )
    max_fw_arity = 1 + max(len(fw["members"]) for fw in seed["food_webs"])
    print()
    print(f"wrote {OUT_PATH}")
    print(f"  regions      : {len(seed['regions'])}")
    print(f"  species      : {n_species}  ({n_with_photo} with photos)")
    print(f"  pollination  : {len(seed['pollination'])}")
    print(f"  mutualism    : {len(seed['mutualism'])}")
    print(f"  parasitism   : {len(seed['parasitism'])}")
    print(f"  predation    : {len(seed['predation'])}")
    print(f"  food_webs    : {len(seed['food_webs'])}  (max arity {max_fw_arity})")
    print(f"  binary/tern. : {n_inter}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
