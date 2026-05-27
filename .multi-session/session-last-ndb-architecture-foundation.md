## Session 2026-05-27/28 (ninth turn) — v2.3.0 shipped: atoms ARE nDB data, N-ary "contains", honest PDBx comparison

Started with the v2.2 prompt (AlphaFold integration + 3D viewer); ended
shipping v2.3 after the user repeatedly pushed for honesty about what
nDB actually stores vs. what was being claimed.

### What landed

Tag `v2.3.0` pushed, GitHub release at
<https://github.com/goldrag1/nDB/releases/tag/v2.3.0>. **488 Rust
tests + 12 Python tests** pass sequentially. Clippy clean.

### Final commits this turn (newest first)

| SHA | Subject |
|---|---|
| `96ad0bc` | docs: README v2.3 status — atoms in nDB + N-ary contains + u32 arity |
| `3ddccf5` | fix(explorer): split KPI table into per-protein vs engine/schema sections |
| `2f9b907` | fix(explorer): split storage KPI into honest atomic-vs-metadata-vs-relations rows |
| `e48e041` | feat(explorer): add operational KPI table to the PDBx-vs-nDB comparison |
| `a97e6d9` | feat(explorer): atoms ARE the structure — drop CIF blob, rebuild PDB on demand, rewrite comparison |
| `9df32bc` | feat: collapse reified binary edges into N-ary "contains" hyperedges + bump engine arity to u32 |
| `3199429` | feat(explorer): persistent nDB across runs + per-atom entities + honest PDBx comparison |
| `e74cd00` | feat(explorer): side-by-side PDBx-vs-nDB comparison in the right sidebar |
| `1032c40` | feat(explorer): top-bar layout + nDB data-model explainer + 5 high-impact features |
| `5044b95` | feat(explorer): store AlphaFold coordinates IN nDB, not just metadata |
| `e75991d` | feat(explorer): three-column layout — protein controls / 3D / live nDB record |
| `dbbabe3` | fix(explorer): structure-only by default + protein picker + opt-in graph view |
| `75e3434` | fix(explorer): auto-load KRAS structure + pin every node on cooldown |
| `8ed46d6` | fix(explorer): no-cache headers on static server |
| `566929a` | fix(explorer): kill constant particle motion at rest + freeze force layout |
| `19d7594` | chore(explorer): scrub remaining literature-graph references from UI + docs |
| `0c3a3e6` | refactor(explorer): drop literature dataset — alphafold_nDB is about structures |
| `8314e3c` | docs(alphafold_nDB): science-friendly landing page + repro cheatsheet |
| `f185a22` | feat(explorer): v2.2 §D — NGL 3D folding renderer + nDB model pane |
| `3f8438d` | feat(explorer): v2.2 §C — residue-level hypergraph + N-ary motifs |
| `7d2a1c4` | feat(explorer): v2.2 §B — live AlphaFold-DB fetch from the sidebar |
| `3d38507` | feat(explorer): v2.2 §A — AlphaFold pLDDT confidence overlay |

### Architectural decisions captured this turn

| Concern | Decision | Why |
|---|---|---|
| Hyperedge arity width | u8 → u32 (`record.rs`). FORMAT_VERSION 1 → 2 | u8::MAX = 255 silently capped the "N-dimensional" pitch. KRAS has 1517 atoms → arity 1518 needed. u32 ≈ 4.3 B fillers, +3 bytes per hyperedge header. |
| CIF blob storage | NOT stored in nDB | Storing `PROP_CIF_BYTES` (180 KB) PLUS atom entities (300 KB) was 3× duplication of atomic data. Atom entities are the canonical store; PDB is rebuilt in memory from them on subsequent loads. |
| "Contains" relationship shape | ONE arity-(N+1) hyperedge per protein, not N binary edges | The whole point of nDB's N-ary support. 1517 reified `atom_of` edges → 1 `protein_atoms` edge with 1518 fillers. Same fix for residues (78 → 5). |
| Persistence | DB on `/tmp/v22-explorer-ndb` survives program restarts | The example seeds only on first boot (`count_entities_of_type(engine, T_PROTEIN) == 0`). All committed data survives — atoms, residues, motifs, AF-DB fetches. |
| Atom decomposition timing | Auto-runs on first protein view, in background | One commit pipeline; idempotent (skip-probe checks `proteinHasAtomEntities`). User-facing "Warm atoms" button does the same flow for every protein. |
| Structure reconstruction format | PDB, not mmCIF | Hand-rolled mmCIF tripped NGL's parser ("render failed: undefined"). PDB's fixed-column format is robust; NGL parses both interchangeably. |
| Comparison framing | Per-protein KPIs (live counts) + Engine/schema KPIs (shared across all proteins) | Single table was misleading — schema dictionary (16 MB) is shared, not per-protein. Two tables clarify what's amortised vs paid per entry. |
| What's honestly claimed | Storage estimates flagged "est." or measured. Million-protein speed: "not benchmarked here, neither side." | User pushed back when comparisons were marketing-y. Estimates need provenance; unmeasured claims need to say so. |

### Honest-disagreement points from this session

The user caught several misleading framings I had to fix:

1. **"3 records vs 1847 rows"** was structurally true but practically misleading — the 1847 PDBx rows lived inside the CIF Bytes blob in nDB. Honest fix: drop the blob, commit atoms.
2. **"1517 atom_of edges"** was the reified-binary anti-pattern nDB exists to avoid. Honest fix: 1 N-ary contains hyperedge.
3. **"Storage 180 KB CIF vs 297 KB nDB"** compared atoms-only to everything-including-metadata. Honest fix: split into atomic / metadata / relations rows.
4. **"Schema units: ~70 categories (16 MB)"** read like per-protein. Honest fix: split table into per-protein vs shared engine/schema KPIs.

User feedback throughout was a steady "be more honest about what you're claiming" — exactly the right kind of pushback for a research project.

### nDB on-disk shape after one warmed KRAS

```
/tmp/v22-explorer-ndb/
├── CURRENT, LOCK, MANIFEST-...
├── 000001.ndb       (proteins + genes + pathways + residues + motifs + complexes + encodes)
├── 000002.ndb       (KRAS atoms + protein_atoms + protein_residues)
└── *.idx            block-index sidecars
Total: ~2.1 MB for 20 proteins, 1 with full atomic detail (1517 atoms).
```

Per-record-kind breakdown after one warmed KRAS:

```
entity     type=  1 (protein)   count=20
entity     type=  2 (gene)      count=5
entity     type=  3 (pathway)   count=4
entity     type=  6 (residue)   count=78
entity     type=  7 (atom)      count=1517
hyper_edge type=100 (complex)              count=6   max-arity=5
hyper_edge type=101 (encodes)              count=5   max-arity=2
hyper_edge type=110 (catalytic_triad)      count=1   max-arity=3
hyper_edge type=111 (disulfide_bond)       count=3   max-arity=2
hyper_edge type=113 (zinc_finger)          count=1   max-arity=4
hyper_edge type=114 (alpha_helix)          count=1   max-arity=16
hyper_edge type=115 (beta_sheet_pair)      count=2   max-arity=20
hyper_edge type=116 (protein_residues)     count=5   max-arity=44
hyper_edge type=117 (protein_atoms)        count=1   max-arity=1518
TOTAL: 1649 records
```

That spread of arities (2 to 1518) inside one record kind is what
"N-dimensional database" means in practice.

### Bugs caught + fixed inline this turn

1. **`pkill -f "examples/v22_explorer"` killed itself** — self-kill trap; per `~/.claude/rules/shell-quirks.md`. Switched to `pgrep + exclude $$ + kill by PID`.
2. **CIF colour palette inverted** — NGL's `bfactor` colour scheme with `colorReverse: true` rendered KRAS's G-domain RED (low confidence) instead of BLUE (high confidence). Removed `colorReverse`, pinned a 4-stop colour scale matching the AF-DB palette.
3. **CSS grid 1fr/1fr blocked by canvas intrinsic sizes** — `#right-pane > * { min-height: 0; min-width: 0 }` is the standard fix. Without it, the 3D pane collapsed to 1px tall.
4. **NGL canvas z-index over the model pane** — model pane was invisible until I pinned `z-index: 10`.
5. **3d-force-graph never cooled** — without `cooldownTicks(300)` + `onEngineStop` pinning fx/fy/fz, nodes micro-drifted forever; user perceived this as "3D dots jumping out".
6. **Static server didn't send Cache-Control headers** — browser cached the HTML; "Hard refresh" became required. Added `Cache-Control: no-store, no-cache, must-revalidate` on every response.
7. **`pkill -f` matching loop killed the shell itself** — recurrence of the `shell-quirks.md` self-kill trap.
8. **Engine `arity = u8::try_from(...)` blew up at 1518** — see Architectural Decisions table above. Format bump 1 → 2.
9. **Hand-rolled mmCIF tripped NGL parser** — switched to PDB output.
10. **Comparison row mixing per-protein with shared-schema costs** — user caught the 16 MB schema dictionary being framed as if it were per-protein.

### Next session entry point

v2.3 is shipped + tagged + released. The major open improvement
buckets identified during this session (none shipped — intentional
follow-up candidates):

1. **Real million-protein benchmark** — the engine has architecture
   designed for O(log N) lookups but it's not measured at scale.
   Existing bench harness at `/home/long/long/rust/` can be scaled
   to 1M random protein-shaped entities; would back the "speed"
   claims in the KPI table with real numbers.
2. **Residue entities for ALL proteins** (not just the 5 showcase) —
   currently non-showcase proteins have atoms but no residue layer.
3. **Per-protein helix/sheet motif hyperedges** committed from NGL's
   derived SS for every protein. Currently NGL re-derives at render
   time (visually nothing's lost), but they're not in nDB.
4. **Bond hyperedges** — covalent, disulfide (partly there), hydrogen,
   salt-bridge.
5. **Pfam/InterPro domain entities** + `has_domain` hyperedges.
6. **PAE matrix** per protein as `Value::Vector` or 2D-indexed property.
7. **Chain entities** for multi-chain proteins + `protein_chains` hyperedge.
8. **Variant entities** for mutations + `affects_residue` hyperedge.
9. **Expose actual SSTable file sizes via a wire endpoint** so the
   storage KPI can show real disk bytes instead of estimates.
10. **Forward-compat reader for v1 record format** — currently v2 readers
    can't open v1 databases. Probably not worth doing unless someone
    has v1 data to migrate.

### Evolution score this turn

- 22 commits (full v2.2 then v2.3 sequence)
- +1 engine format-version bump (v1 → v2)
- +1 engine API change (`HyperEdgeRecord.encode/decode`: u8 → u32 arity)
- +1 new entity type (T_ATOM = 7)
- +2 new hyperedge types repurposed (T_PROTEIN_RESIDUES, T_PROTEIN_ATOMS)
- +10 atom properties (PROP_ATOM_NAME, _ELEMENT, _SERIAL, _X, _Y, _Z, _BFACTOR, _CHAIN, _RES_POS, _RES_NAME)
- 1 property dropped (PROP_CIF_BYTES — atoms replace it)
- 1 new science-facing landing page + reproducibility doc
- 4 cross-project rules NOT promoted (everything project-specific)

---

