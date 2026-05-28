#!/usr/bin/env python3
"""
Build seed.json for chemistry_ndb.

Reads tools/chemistry/reactions.py (curated reaction list), collects
every unique compound + catalyst + solvent name, resolves each to its
canonical SMILES + molecular formula + InChIKey via PubChem PUG REST.
Caches each lookup so re-runs are instant.

Output: crates/ndb-renderer/examples/chemistry_explorer/seed.json
"""
from __future__ import annotations

import json
import pathlib
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
CACHE_DIR = REPO_ROOT / ".demo-data" / "source-data" / "chemistry"
OUT_PATH  = REPO_ROOT / "crates" / "ndb-renderer" / "examples" / "chemistry_explorer" / "seed.json"

sys.path.insert(0, str(REPO_ROOT / "tools" / "chemistry"))
from reactions import REACTIONS, PATHWAYS  # noqa: E402

# ── Manual SMILES overrides for compounds PubChem either lacks or returns
#    something inconvenient for (e.g. proteins, polymers). We use these
#    when PubChem resolution either fails or returns something we don't
#    want to draw (polymer asterisks, peptide chains, etc.).
MANUAL_SMILES: dict[str, dict] = {
    # Polymers — SmilesDrawer can render unit cells.
    "polyethylene":            {"smiles": "CCCCCCCCCC",                    "formula": "(C2H4)n", "kind": "polymer"},
    "nylon-6,6 unit":          {"smiles": "O=C(N)CCCCCC(=O)NCCCCCCN",     "formula": "C12H22N2O2 (repeat)", "kind": "polymer"},
    # Enzymes / proteins — represented as opaque labels (no structure).
    "RuBisCO":                                       {"kind": "enzyme",  "ec": "EC 4.1.1.39"},
    "hexokinase":                                    {"kind": "enzyme",  "ec": "EC 2.7.1.1"},
    "phosphoglucose isomerase":                      {"kind": "enzyme",  "ec": "EC 5.3.1.9"},
    "phosphofructokinase-1":                         {"kind": "enzyme",  "ec": "EC 2.7.1.11"},
    "fructose-bisphosphate aldolase":                {"kind": "enzyme",  "ec": "EC 4.1.2.13"},
    "triose phosphate isomerase":                    {"kind": "enzyme",  "ec": "EC 5.3.1.1"},
    "glyceraldehyde 3-phosphate dehydrogenase":      {"kind": "enzyme",  "ec": "EC 1.2.1.12"},
    "phosphoglycerate kinase":                       {"kind": "enzyme",  "ec": "EC 2.7.2.3"},
    "phosphoglycerate mutase":                       {"kind": "enzyme",  "ec": "EC 5.4.2.11"},
    "enolase":                                       {"kind": "enzyme",  "ec": "EC 4.2.1.11"},
    "pyruvate kinase":                               {"kind": "enzyme",  "ec": "EC 2.7.1.40"},
    "citrate synthase":                              {"kind": "enzyme",  "ec": "EC 2.3.3.1"},
    "aconitase":                                     {"kind": "enzyme",  "ec": "EC 4.2.1.3"},
    "isocitrate dehydrogenase":                      {"kind": "enzyme",  "ec": "EC 1.1.1.41"},
    "alpha-ketoglutarate dehydrogenase complex":     {"kind": "enzyme",  "ec": "EC 1.2.4.2"},
    "succinyl-CoA synthetase":                       {"kind": "enzyme",  "ec": "EC 6.2.1.4"},
    "succinate dehydrogenase":                       {"kind": "enzyme",  "ec": "EC 1.3.5.1"},
    "fumarase":                                      {"kind": "enzyme",  "ec": "EC 4.2.1.2"},
    "malate dehydrogenase":                          {"kind": "enzyme",  "ec": "EC 1.1.1.37"},
    # Industrial catalysts where PubChem records aren't useful structures.
    "tetrakis(triphenylphosphine)palladium(0)":      {"kind": "catalyst","note": "Pd(PPh3)4"},
    "titanium tetrachloride":                        {"smiles": "Cl[Ti](Cl)(Cl)Cl",          "formula": "TiCl4",      "kind": "compound"},
    "vanadium(V) oxide":                             {"smiles": "O=[V](=O)O[V](=O)=O",        "formula": "V2O5",       "kind": "compound"},
    "aluminum chloride":                             {"smiles": "Cl[Al](Cl)Cl",               "formula": "AlCl3",      "kind": "compound"},
    "palladium(II) chloride":                        {"smiles": "Cl[Pd]Cl",                   "formula": "PdCl2",      "kind": "compound"},
    "iron":                                          {"smiles": "[Fe]",                       "formula": "Fe",         "kind": "compound"},
    # Diatomics / common species PubChem returns the right thing for, but
    # we override for compactness.
    "hydrogen":          {"smiles": "[H][H]",   "formula": "H2",       "kind": "compound"},
    "nitrogen":          {"smiles": "N#N",      "formula": "N2",       "kind": "compound"},
    "oxygen":            {"smiles": "O=O",      "formula": "O2",       "kind": "compound"},
    "chlorine":          {"smiles": "ClCl",     "formula": "Cl2",      "kind": "compound"},
    "ammonia":           {"smiles": "N",        "formula": "NH3",      "kind": "compound"},
    "water":             {"smiles": "O",        "formula": "H2O",      "kind": "compound"},
    "methane":           {"smiles": "C",        "formula": "CH4",      "kind": "compound"},
    "ethylene":          {"smiles": "C=C",      "formula": "C2H4",     "kind": "compound"},
    "carbon dioxide":    {"smiles": "O=C=O",    "formula": "CO2",      "kind": "compound"},
    "sodium hydroxide":  {"smiles": "[Na+].[OH-]",         "formula": "NaOH",  "kind": "compound"},
    "hydrogen chloride": {"smiles": "Cl",                  "formula": "HCl",   "kind": "compound"},
    "sodium chloride":   {"smiles": "[Na+].[Cl-]",         "formula": "NaCl",  "kind": "compound"},
    "sodium":            {"smiles": "[Na]",                "formula": "Na",    "kind": "compound"},
    "sulfuric acid":     {"smiles": "OS(=O)(=O)O",         "formula": "H2SO4", "kind": "compound"},
    "iron(III) oxide":   {"smiles": "O=[Fe]O[Fe]=O",       "formula": "Fe2O3", "kind": "compound"},
    "sulfur dioxide":    {"smiles": "O=S=O",               "formula": "SO2",   "kind": "compound"},
    "sulfur trioxide":   {"smiles": "O=S(=O)=O",           "formula": "SO3",   "kind": "compound"},
    "calcium carbonate": {"smiles": "[Ca+2].[O-]C(=O)[O-]","formula": "CaCO3", "kind": "compound"},
    "calcium oxide":     {"smiles": "[Ca]=O",              "formula": "CaO",   "kind": "compound"},
    "phosphate":         {"smiles": "OP(=O)(O)O",          "formula": "Pi",    "kind": "compound"},
    "NAD+":              {"smiles": "[NH2]C(=O)c1ccc[n+](C)c1", "formula": "NAD+ (sketch)", "kind": "cofactor"},
    "NADH":              {"smiles": "[NH2]C(=O)C1CC=CN1C",      "formula": "NADH (sketch)", "kind": "cofactor"},
    "FAD":               {"kind": "cofactor", "note": "C27H33N9O15P2"},
    "FADH2":             {"kind": "cofactor", "note": "C27H35N9O15P2"},
    "GDP":               {"kind": "cofactor", "note": "C10H15N5O11P2"},
    "GTP":               {"kind": "cofactor", "note": "C10H16N5O14P3"},
    "coenzyme A":        {"kind": "cofactor", "note": "C21H36N7O16P3S"},
    "acetyl-CoA":        {"kind": "cofactor", "note": "C23H38N7O17P3S"},
    "succinyl-CoA":      {"kind": "cofactor", "note": "C25H40N7O19P3S"},
    "adenosine triphosphate": {"kind": "cofactor", "note": "C10H16N5O13P3"},
    "adenosine diphosphate":  {"kind": "cofactor", "note": "C10H15N5O10P2"},
    # Misc industrial / unusual that PubChem resolves badly.
    "magnesium bromide hydroxide": {"smiles": "[Mg+2].[OH-].[Br-]", "formula": "MgBrOH", "kind": "compound"},
    "methylmagnesium bromide":     {"smiles": "C[Mg]Br",          "formula": "CH3MgBr",  "kind": "compound"},
    "triethylaluminum":            {"smiles": "CC[Al](CC)CC",     "formula": "Al(C2H5)3","kind": "compound"},
    "boric acid":                  {"smiles": "OB(O)O",           "formula": "B(OH)3",   "kind": "compound"},
    "phenylboronic acid":          {"smiles": "OB(O)c1ccccc1",    "formula": "C6H7BO2",  "kind": "compound"},
    "potassium bromide":           {"smiles": "[K+].[Br-]",       "formula": "KBr",      "kind": "compound"},
    "potassium carbonate":         {"smiles": "[K+].[K+].[O-]C(=O)[O-]", "formula": "K2CO3", "kind": "compound"},
    "sodium carbonate":            {"smiles": "[Na+].[Na+].[O-]C(=O)[O-]", "formula": "Na2CO3", "kind": "compound"},
    "sodium bromide":              {"smiles": "[Na+].[Br-]",      "formula": "NaBr",     "kind": "compound"},
    "lithium bromide":             {"smiles": "[Li+].[Br-]",      "formula": "LiBr",     "kind": "compound"},
    "butyllithium":                {"smiles": "CCCC[Li]",         "formula": "C4H9Li",   "kind": "compound"},
    "potassium bromate":           {"smiles": "[K+].[O-]Br(=O)=O","formula": "KBrO3",    "kind": "compound"},
    "cerium(IV)":                  {"kind": "compound", "note": "Ce4+ ion (in Ce(SO4)2)"},
    "bromine":                     {"smiles": "BrBr",             "formula": "Br2",      "kind": "compound"},
    "fructosylamine":              {"smiles": "OCC(O)C(O)C(O)C(O)CN",      "formula": "C7H17NO5",  "kind": "compound"},
    "sodium stearate":             {"smiles": "[Na+].CCCCCCCCCCCCCCCCCC(=O)[O-]", "formula": "C18H35NaO2", "kind": "compound"},
    "tristearin":                  {"smiles": "CCCCCCCCCCCCCCCCCC(=O)OCC(OC(=O)CCCCCCCCCCCCCCCCC)COC(=O)CCCCCCCCCCCCCCCCC", "formula": "C57H110O6", "kind": "compound"},
    "bromomalonic acid":           {"smiles": "OC(=O)C(Br)C(=O)O", "formula": "C3H3BrO4", "kind": "compound"},
    "malonic acid":                {"smiles": "OC(=O)CC(=O)O",     "formula": "C3H4O4",   "kind": "compound"},
    "diethyl ether":               {"smiles": "CCOCC",             "formula": "C4H10O",   "kind": "compound"},
    "tetrahydrofuran":             {"smiles": "C1CCOC1",           "formula": "C4H8O",    "kind": "compound"},
    "dichloromethane":             {"smiles": "ClCCl",             "formula": "CH2Cl2",   "kind": "compound"},
    "hexane":                      {"smiles": "CCCCCC",            "formula": "C6H14",    "kind": "compound"},
    # Krebs / glycolysis intermediates — PubChem has them but they're
    # large; pre-set the SMILES so build_seed doesn't make 30 API calls.
    "glucose":                       {"smiles": "OCC1OC(O)C(O)C(O)C1O", "formula": "C6H12O6",   "kind": "compound"},
    "glucose 6-phosphate":           {"smiles": "OC1OC(COP(=O)(O)O)C(O)C(O)C1O", "formula": "C6H13O9P", "kind": "compound"},
    "fructose 6-phosphate":          {"smiles": "OCC1(O)OC(COP(=O)(O)O)C(O)C1O", "formula": "C6H13O9P", "kind": "compound"},
    "fructose 1,6-bisphosphate":     {"smiles": "OP(=O)(O)OCC1(O)OC(COP(=O)(O)O)C(O)C1O", "formula": "C6H14O12P2", "kind": "compound"},
    "dihydroxyacetone phosphate":    {"smiles": "OCC(=O)COP(=O)(O)O", "formula": "C3H7O6P", "kind": "compound"},
    "glyceraldehyde 3-phosphate":    {"smiles": "OCC(O)COP(=O)(O)O", "formula": "C3H7O6P", "kind": "compound"},
    "1,3-bisphosphoglycerate":       {"smiles": "OP(=O)(O)OCC(O)C(=O)OP(=O)(O)O", "formula": "C3H8O10P2", "kind": "compound"},
    "3-phosphoglycerate":            {"smiles": "OC(C(=O)O)COP(=O)(O)O", "formula": "C3H7O7P", "kind": "compound"},
    "2-phosphoglycerate":            {"smiles": "OCC(OP(=O)(O)O)C(=O)O", "formula": "C3H7O7P", "kind": "compound"},
    "phosphoenolpyruvate":           {"smiles": "OP(=O)(O)OC(=C)C(=O)O", "formula": "C3H5O6P", "kind": "compound"},
    "pyruvate":                      {"smiles": "CC(=O)C(=O)[O-]", "formula": "C3H3O3-",      "kind": "compound"},
    "oxaloacetate":                  {"smiles": "OC(=O)CC(=O)C(=O)O", "formula": "C4H4O5",    "kind": "compound"},
    "citrate":                       {"smiles": "OC(=O)CC(O)(C(=O)O)CC(=O)O", "formula": "C6H8O7", "kind": "compound"},
    "isocitrate":                    {"smiles": "OC(C(=O)O)C(C(=O)O)CC(=O)O", "formula": "C6H8O7", "kind": "compound"},
    "alpha-ketoglutarate":           {"smiles": "OC(=O)CCC(=O)C(=O)O",  "formula": "C5H6O5", "kind": "compound"},
    "succinate":                     {"smiles": "OC(=O)CCC(=O)O",       "formula": "C4H6O4", "kind": "compound"},
    "fumarate":                      {"smiles": "OC(=O)/C=C/C(=O)O",    "formula": "C4H4O4", "kind": "compound"},
    "malate":                        {"smiles": "OC(=O)C(O)CC(=O)O",    "formula": "C4H6O5", "kind": "compound"},
    "glycine":                       {"smiles": "NCC(=O)O",             "formula": "C2H5NO2", "kind": "compound"},
    "ethanol":                       {"smiles": "CCO",                  "formula": "C2H6O",  "kind": "compound"},
    "acetic acid":                   {"smiles": "CC(=O)O",              "formula": "C2H4O2", "kind": "compound"},
    "ethyl acetate":                 {"smiles": "CC(=O)OCC",            "formula": "C4H8O2", "kind": "compound"},
    "salicylic acid":                {"smiles": "OC(=O)c1ccccc1O",      "formula": "C7H6O3", "kind": "compound"},
    "acetic anhydride":              {"smiles": "CC(=O)OC(=O)C",        "formula": "C4H6O3", "kind": "compound"},
    "aspirin":                       {"smiles": "CC(=O)Oc1ccccc1C(=O)O", "formula": "C9H8O4", "kind": "compound"},
    "1,3-butadiene":                 {"smiles": "C=CC=C",                "formula": "C4H6",   "kind": "compound"},
    "cyclohexene":                   {"smiles": "C1=CCCCC1",             "formula": "C6H10",  "kind": "compound"},
    "benzene":                       {"smiles": "c1ccccc1",              "formula": "C6H6",   "kind": "compound"},
    "acetyl chloride":               {"smiles": "CC(=O)Cl",              "formula": "C2H3ClO","kind": "compound"},
    "acetophenone":                  {"smiles": "CC(=O)c1ccccc1",        "formula": "C8H8O",  "kind": "compound"},
    "bromobenzene":                  {"smiles": "Brc1ccccc1",            "formula": "C6H5Br", "kind": "compound"},
    "biphenyl":                      {"smiles": "c1ccc(-c2ccccc2)cc1",   "formula": "C12H10", "kind": "compound"},
    "formaldehyde":                  {"smiles": "C=O",                   "formula": "CH2O",   "kind": "compound"},
    "benzaldehyde":                  {"smiles": "O=Cc1ccccc1",           "formula": "C7H6O",  "kind": "compound"},
    "styrene":                       {"smiles": "C=Cc1ccccc1",           "formula": "C8H8",   "kind": "compound"},
    "methyltriphenylphosphonium bromide": {"smiles": "C[P+](c1ccccc1)(c1ccccc1)c1ccccc1.[Br-]", "formula": "C19H18BrP", "kind": "compound"},
    "triphenylphosphine oxide":      {"smiles": "O=P(c1ccccc1)(c1ccccc1)c1ccccc1",       "formula": "C18H15OP", "kind": "compound"},
    "methylamine":                   {"smiles": "CN",                    "formula": "CH5N",   "kind": "compound"},
    "acetamide":                     {"smiles": "CC(=O)N",               "formula": "C2H5NO", "kind": "compound"},
    "acetone":                       {"smiles": "CC(=O)C",               "formula": "C3H6O",  "kind": "compound"},
    "dimethylamine":                 {"smiles": "CNC",                   "formula": "C2H7N",  "kind": "compound"},
    "1-(dimethylamino)-3-butanone":  {"smiles": "CN(C)CCC(=O)C",         "formula": "C6H13NO","kind": "compound"},
    "adipic acid":                   {"smiles": "OC(=O)CCCCC(=O)O",      "formula": "C6H10O4","kind": "compound"},
    "hexamethylenediamine":          {"smiles": "NCCCCCCN",              "formula": "C6H16N2","kind": "compound"},
}


def slug(name: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")


def cache_path(name: str) -> pathlib.Path:
    return CACHE_DIR / f"{slug(name)}.json"


def pubchem_lookup(name: str) -> dict:
    """Best-effort PubChem PUG REST lookup for SMILES + formula.
    Returns {} on failure (caller falls back to MANUAL_SMILES or omits the structure)."""
    if name in MANUAL_SMILES:
        return MANUAL_SMILES[name]
    p = cache_path(name)
    if p.exists():
        try:
            return json.loads(p.read_text())
        except Exception:
            pass

    url = ("https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/name/"
           + urllib.parse.quote(name)
           + "/property/CanonicalSMILES,MolecularFormula,InChIKey/JSON")
    try:
        req = urllib.request.Request(url, headers={
            "User-Agent": "nDB chemistry_ndb demo seed builder (+github.com/goldrag1/nDB)"
        })
        with urllib.request.urlopen(req, timeout=15) as r:
            d = json.loads(r.read())
        props = d["PropertyTable"]["Properties"][0]
        result = {
            "smiles":   props.get("CanonicalSMILES"),
            "formula":  props.get("MolecularFormula"),
            "inchikey": props.get("InChIKey"),
            "kind":     "compound",
        }
    except Exception as e:
        print(f"  ! pubchem failed for {name!r}: {e}", file=sys.stderr)
        result = {}
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(result))
    time.sleep(0.3)  # polite to PubChem
    return result


def main():
    CACHE_DIR.mkdir(parents=True, exist_ok=True)

    # Collect unique compound names by role.
    compound_names:  set[str] = set()
    catalyst_names:  set[str] = set()
    solvent_names:   set[str] = set()
    for rx in REACTIONS:
        for stoich, name in rx["reactants"]:  compound_names.add(name)
        for stoich, name in rx["products"]:   compound_names.add(name)
        if rx.get("catalyst"): catalyst_names.add(rx["catalyst"])
        if rx.get("solvent"):  solvent_names.add(rx["solvent"])

    # Resolve metadata for each.
    print(f"Resolving {len(compound_names)} compounds…")
    compounds = []
    for name in sorted(compound_names):
        meta = pubchem_lookup(name)
        compounds.append({
            "name":     name,
            "smiles":   meta.get("smiles"),
            "formula":  meta.get("formula"),
            "inchikey": meta.get("inchikey"),
            "kind":     meta.get("kind", "compound"),
            "note":     meta.get("note"),
            "ec":       meta.get("ec"),
        })
        sm = meta.get("smiles")
        if sm: print(f"  ✓ {name:38s} → {sm[:50]}")
        else:  print(f"  ⊗ {name:38s} → (no structure — {meta.get('kind') or 'unknown'})")

    print(f"Resolving {len(catalyst_names)} catalysts…")
    catalysts = []
    for name in sorted(catalyst_names):
        meta = pubchem_lookup(name)
        catalysts.append({
            "name":     name,
            "smiles":   meta.get("smiles"),
            "formula":  meta.get("formula"),
            "inchikey": meta.get("inchikey"),
            "kind":     meta.get("kind", "catalyst"),
            "ec":       meta.get("ec"),
            "note":     meta.get("note"),
        })

    print(f"Resolving {len(solvent_names)} solvents…")
    solvents = []
    for name in sorted(solvent_names):
        meta = pubchem_lookup(name)
        solvents.append({
            "name":     name,
            "smiles":   meta.get("smiles"),
            "formula":  meta.get("formula"),
            "kind":     meta.get("kind", "solvent"),
        })

    # Emit reactions verbatim (keep the Python data intact in JSON).
    reactions_out = []
    for rx in REACTIONS:
        reactions_out.append(dict(rx))   # shallow copy is enough; only str/list/dict children

    # Pathways: group reactions by `pathway` field.
    pathways = []
    for pathway_name in PATHWAYS:
        members = [rx for rx in REACTIONS if rx.get("pathway") == pathway_name]
        members.sort(key=lambda rx: rx.get("pathway_order", 0))
        pathways.append({
            "name":     pathway_name,
            "n_steps":  len(members),
            "reactions": [m["name"] for m in members],
            "shape":    "cycle" if pathway_name == "Krebs cycle" else "linear",
        })

    out = {
        "schema_note":  "chemistry_ndb seed v1. Hand-curated reactions; SMILES + formulae from PubChem.",
        "counts": {
            "compounds":  len(compounds),
            "catalysts":  len(catalysts),
            "solvents":   len(solvents),
            "reactions":  len(reactions_out),
            "pathways":   len(pathways),
        },
        "compounds": compounds,
        "catalysts": catalysts,
        "solvents":  solvents,
        "reactions": reactions_out,
        "pathways":  pathways,
    }
    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_text(json.dumps(out, indent=1) + "\n", encoding="utf-8")
    print(f"\nwrote {OUT_PATH}  ({OUT_PATH.stat().st_size // 1024} KB)")
    for k, v in out["counts"].items():
        print(f"  {k:11s} {v}")


if __name__ == "__main__":
    main()
