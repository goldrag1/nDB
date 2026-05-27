# Next session — AlphaFold + protein folding into nDB

Continuing nDB after v2.1.0 + the v2.2 explorer preview. v2.1 is
tagged + released; the v2.2 explorer (3D force-directed graph, CORS,
live editing, signal-flow viz) shipped as `c359a16`.

This session adds **Nobel-worthy data + viz**: AlphaFold confidence,
live AlphaFold-DB lookup, residue-level hyperedges, and a true 3D
protein-folding renderer. Four deliverables, in order; each commits
independently and keeps tests green.

Repo: `/home/long/long/nDB-ndimemsion-database`
GitHub: `https://github.com/goldrag1/nDB` (latest tag `v2.1.0`, branch
`main`, latest commit `c359a16`)

## Read first

1. `README.md` — current shipped state (v2.1)
2. `docs/superpowers/specs/2026-05-27-v2-1-working-spec.md` — what
   v2.1 promised + delivered
3. `.multi-session/session-last-ndb-architecture-foundation.md`
   — most recent turn is the v2.2 explorer wrap-up
4. `crates/ndb-renderer/examples/v22_explorer.rs` — the explorer
   wiring (seeds data, runs ndb-server + static HTTP). You will
   extend this rather than start fresh.
5. `docs/explorer/index.html` — the SPA. You will extend it for
   AlphaFold confidence overlay + (later) the residue viz.
6. `cargo test --workspace --no-fail-fast` should report 23 result
   groups all `ok` (484+ Rust + 12 Python tests) and `cargo clippy
   --workspace --all-targets -- -D warnings` clean. Confirm before
   changing anything.

## Deliverables (sequenced)

### A. Curated AlphaFold-confidence overlay (~half day)

Pre-seed the v22 explorer with real **AlphaFold-DB pLDDT confidence
scores** for the 15 proteins in the existing dataset. Confidence
becomes a node property + a complex-edge property. The viz colours +
particle behaviour reflect confidence.

#### Schema additions

- New entity property `PROP_PLDDT_MEAN: f64` (id 36) — average pLDDT
  across the structure, 0–100
- New entity property `PROP_PLDDT_BUCKET: String` (id 37) — one of
  `"very_high"` (>90), `"confident"` (>70), `"low"` (>50), `"very_low"` (≤50)
- New hyperedge property `PROP_COMPLEX_CONFIDENCE: f64` (id 38) —
  synthesised from member pLDDT means (mean × 0.8 + min × 0.2 is a
  decent proxy when we don't have AlphaFold-Multimer's ipTM)

#### Data source

Hardcode the constants in `v22_explorer.rs` for the 15 seed proteins
from the existing dataset. Real values (rounded to 1 decimal):

```rust
// Mean pLDDT from AlphaFold-DB v4 (May 2024). Hardcoded to avoid
// a network dependency at boot; deliverable B adds the live fetch.
const PLDDT_BY_NAME: &[(&str, f64)] = &[
    ("P53",   84.7),   // P04637 — disordered N+C tails drag the average down
    ("MDM2",  79.2),
    ("ATM",   90.4),
    ("CHK2",  88.1),
    ("BRCA1", 71.3),   // very large with disordered regions
    ("BRCA2", 65.8),
    ("AKT1",  92.1),
    ("MTOR",  87.6),
    ("ULK1",  85.0),
    ("BECN1", 88.3),
    ("LC3",   95.7),
    ("ATG7",  91.2),
    ("PI3K",  86.9),
    ("PTEN",  93.4),
    ("KRAS",  96.8),
];
```

(Verify each before commit — re-fetch from
`https://alphafold.ebi.ac.uk/api/prediction/<UniProt>` to confirm.
The UniProt accessions for our 15 are well-known: P04637, Q00987,
Q13315, O96017, P38398, P51587, P31749, P42345, O75385, Q14457,
Q9GZQ8, O95352, P42336, P60484, P01116.)

#### Viz changes (`docs/explorer/index.html`)

- Node `nodeVal` (size) scaled by pLDDT — high-confidence proteins
  draw larger. 3d-force-graph supports `.nodeVal(n => ...)`.
- Node `nodeColor` modified: keep type colour for hue, modulate
  saturation/alpha by pLDDT bucket so `very_low` proteins appear
  washed out.
- Hyperedge particles: `linkDirectionalParticleSpeed` and
  `linkDirectionalParticleColor` blended with the edge confidence.
- Sidebar: add a confidence-distribution mini-bar (count of proteins
  per bucket). Optional but cheap.

#### Tests

- Engine: seed produces 15 proteins each with `PROP_PLDDT_MEAN` and
  `PROP_PLDDT_BUCKET` set, plus a `PROP_COMPLEX_CONFIDENCE` on every
  complex hyperedge.
- Unit: bucket assignment function (`pLDDT → bucket`) matches the
  AlphaFold-published thresholds (>90, >70, >50).

### B. Live AlphaFold-DB lookup (~1-2 days)

Sidebar gains a **"Fetch & add from AlphaFold-DB"** form. User pastes
a UniProt accession; the example backend proxies to
`alphafold.ebi.ac.uk/api/prediction/<acc>`, parses the JSON, commits
a new protein entity to nDB with the real pLDDT + organism + sequence
length + gene name, then refreshes the graph.

#### Why proxy (not direct fetch from browser)?

EBI's CORS policy on the AlphaFold-DB API may not include our
explorer origin (need to verify; if it does, skip the proxy and call
directly). Add a `/proxy/alphafold/:acc` route on `ndb-server`
(behind a feature flag `--enable-alphafold-proxy` so it's opt-in) OR
add the proxy to the static file server in `v22_explorer.rs`. The
latter is the less invasive path — no changes to ndb-server.

#### Proxy route shape

```
GET /alphafold/<accession>

→ fetches https://alphafold.ebi.ac.uk/api/prediction/<accession>
→ returns the JSON body verbatim with CORS headers added
→ caches the response to /tmp/v22-explorer-ndb/.af-cache/<acc>.json
  so repeated lookups don't hammer EBI
```

#### Commit shape

For a fetched protein at accession `P53` (UniProt P04637):

```json
{
  "kind": "entity",
  "entity_id": "<v7 uuid>",
  "type_id": 1,                    // protein
  "properties": [
    {"prop_id": 30, "value": {"tag": "string", "value": "P53"}},
    {"prop_id": 36, "value": {"tag": "f64", "value": 84.7}},
    {"prop_id": 37, "value": {"tag": "string", "value": "confident"}},
    {"prop_id": 39, "value": {"tag": "string", "value": "P04637"}},  // new PROP_UNIPROT
    {"prop_id": 40, "value": {"tag": "i64", "value": 393}},           // PROP_SEQ_LEN
    {"prop_id": 41, "value": {"tag": "string", "value": "Homo sapiens"}}, // PROP_ORGANISM
  ]
}
```

#### Tests

- Proxy returns the cached response on the second call (assert
  cache file exists after first call; mock the EBI URL with a local
  server in tests).
- Bad accession → 404 from the proxy with a clear error body.
- Successful fetch + commit increments the entity count + the new
  protein appears in `/iter`.

#### Open question (decide at start of session)

If EBI's CORS already permits our origin, skip the proxy entirely.
Test with `curl -i -H "Origin: http://127.0.0.1:9876"
https://alphafold.ebi.ac.uk/api/prediction/P04637` and check the
`Access-Control-Allow-Origin` header.

### C. Residue-level hypergraph (~2-3 days)

The deepest deliverable. Adds **TYPE_RESIDUE** (id 6) and structural
motif hyperedge types. A 300-residue protein becomes 300 entity
records + N motif hyperedges. Demonstrates why hyperedges are the
right shape for structural biology.

#### Schema

- `TYPE_RESIDUE = 6` (entity type)
- New role IDs:
  - `ROLE_RESIDUE_OF: 20` — links a residue to its parent protein
- New hyperedge types:
  - `TYPE_CATALYTIC_TRIAD: 110` — N-ary, typically 3 residues (Ser-His-Asp for serine proteases)
  - `TYPE_DISULFIDE_BOND: 111` — binary, 2 cysteine residues
  - `TYPE_HBOND_NETWORK: 112` — N-ary, ≥3 residues hydrogen-bonded
  - `TYPE_ZINC_FINGER: 113` — N-ary, 4 residues coordinating a Zn²⁺
  - `TYPE_ALPHA_HELIX: 114` — N-ary, residues i…i+n folded into a helix
  - `TYPE_BETA_SHEET_PAIR: 115` — N-ary, two strands of paired residues
- New residue properties:
  - `PROP_RESIDUE_POSITION: i64` (id 50) — 1-indexed position in the chain
  - `PROP_AMINO_ACID: String` (id 51) — 3-letter code ("ALA", "GLY", ...)
  - `PROP_RESIDUE_PLDDT: f64` (id 52) — per-residue confidence
  - `PROP_SECONDARY_STRUCTURE: String` (id 53) — "H" / "E" / "C" (helix/sheet/coil)

#### Data source

Curated dataset for ~5 well-characterised proteins. Hardcode in
`v22_explorer.rs` initially:

- **Trypsin** (catalytic triad Ser195-His57-Asp102; serine protease textbook)
- **TFIIIA finger 1** (zinc finger Cys-Cys-His-His)
- **Insulin A+B chains** (3 disulfide bonds; classic small structure)
- **Myoglobin** (single alpha-helix-dominated structure; iconic)
- **GFP** (beta-barrel of 11 antiparallel sheets — N-ary beta sheet showcase)

For each, hardcode the residue list (3-letter code + position) + the
motif hyperedges that connect them. Real residue counts for these
small/medium proteins: trypsin ~223, TFIIIA ~344 (but we only need
the finger), insulin 51 + 21, myoglobin 153, GFP 238 — total
manageable (~1000 residues entities).

#### Viz changes

- Update the explorer's `max_nodes` cap from 200 → 1500 to accommodate
  residue entities (still well within the hypergraph diagram's
  rendering budget at v2.1's force-directed layout cost).
- New type colour entries in TYPES map for residue + each motif type.
- New "Show residues" toggle in the sidebar — when off, residue
  entities are filtered out client-side and only proteins +
  protein-level hyperedges show (default off so the protein-level
  view stays clean).
- When a protein is clicked, the signal-flow now lights up not just
  the protein-protein complexes but also the residue motifs (the
  catalytic triad of the selected protein highlights as a clear
  3-residue polygon).

#### Tests

- Engine: trypsin seeds 223 residues + 1 catalytic_triad hyperedge of
  arity 3 connecting Ser195/His57/Asp102.
- Insulin seeds 72 residues across 2 chains + 3 disulfide_bond
  hyperedges, each of arity 2.
- Quering `/iter` for `type_id=6` returns the residue entities.

### D. 3D protein-folding renderer (~3-5 days)

The showcase. A **second view** in the explorer that renders the
*actual 3D structure* of a selected protein — ribbon / cartoon / ball-
and-stick — and overlays the nDB hyperedges directly on the 3D model.
Click a residue node in the hypergraph → it highlights in the 3D
structure. Click a catalytic-triad hyperedge → the 3 residues
involved glow in the structure.

#### Library choice

- **NGL Viewer** (`https://nglviewer.org/ngl/`) — actively
  maintained, WebGL, used by RCSB PDB. Reads PDB/CIF/mmCIF directly.
- **3Dmol.js** — alternative, lighter weight but less feature-rich.
- **Mol* (`https://molstar.org`)** — the modern successor, used by
  PDBe. Steeper learning curve but state of the art.

**Recommendation: start with NGL Viewer** for the MVP. If the UI
needs more polish later, swap to Mol*.

#### Data source

PDB / mmCIF files for our 5 curated proteins. Two paths:

1. **Real PDB files** — download from RCSB (`https://files.rcsb.org/download/<id>.cif`):
   - Trypsin: 5PTP (bovine, classic)
   - Insulin: 1MSO (humulin)
   - Myoglobin: 1MBN (sperm whale)
   - GFP: 1EMA
   - TFIIIA: 1TF6
   These are small (10-200 KB each), fast to load, well-known
   structures.

2. **AlphaFold structures** — `alphafold.ebi.ac.uk/files/AF-<acc>-F1-model_v4.cif`.
   Bigger files (full-length predictions), but consistent with our
   AlphaFold theme.

**Recommendation: AlphaFold structures** for thematic consistency
with deliverables A + B. Fetch via the same proxy from deliverable
B; cache to `/tmp/v22-explorer-ndb/.af-cache/<acc>.cif`.

#### UI shape

Split the explorer's main viewport into two panes:
- **Left:** existing 3D hypergraph (force-directed)
- **Right:** new 3D structure view (NGL canvas)

Sidebar adds a "Load structure" button that's enabled when a single
protein entity is selected. Clicking it fetches the CIF and loads
into NGL with default cartoon representation + per-residue pLDDT
colouring (the standard AlphaFold colour scheme: very_high = dark
blue, confident = light blue, low = yellow, very_low = orange).

Hyperedge → 3D bridge:
- Click a `catalytic_triad` hyperedge in the left pane → the 3
  residues in the right pane highlight (NGL: `addRepresentation('ball+stick',
  {sel: '57 or 102 or 195'})`).
- Click a residue node in the left pane → that single residue
  highlights in the right pane + the camera centres on it
  (`structureComponent.autoView('195')`).

Bidirectional:
- Click a residue *in the NGL canvas* (NGL supports click handlers via
  `stage.signals.clicked`) → that residue's nDB entity gets selected
  in the left pane + signal-flow lights up.

#### Tests

- The proxy endpoint fetches + caches CIF files (mirrors deliverable
  B's pattern).
- Loading `1MBN.cif` into the explorer doesn't throw any console
  errors and displays 153 residues.
- Selecting trypsin and clicking its catalytic_triad hyperedge
  highlights residues 57, 102, 195 in the NGL canvas.

## Sequencing rationale

| # | Deliverable | Effort | Depends on |
|---|---|---|---|
| A | AlphaFold confidence overlay | ~0.5d | — |
| B | Live AlphaFold-DB lookup | ~1-2d | A (uses the same property schema) |
| C | Residue-level hypergraph | ~2-3d | A (uses pLDDT per-residue) |
| D | 3D folding renderer | ~3-5d | B (CIF fetch via proxy) + C (residue → 3D mapping) |

Commit each deliverable independently. After D, the explorer is a
proper structural-biology tool — a Nobel-worthy demonstration of
why a hyperedge-native database matches structural reality.

## Constraints (unchanged from v2.1)

- Single-process engine model. SharedEngine wraps Engine for thread
  safety; no platform refactor.
- Wire protocol unchanged. CORS already shipped in `f545bbb`.
- v2.0+ on-disk format unchanged; v2.2 explorer DB at
  `/tmp/v22-explorer-ndb` is rebuilt fresh on every example run.
- `cargo test --workspace --no-fail-fast` + `cargo clippy --workspace
  --all-targets -- -D warnings` stay green after every commit.
- Network deps: NGL Viewer + 3d-force-graph from unpkg CDN are OK
  (already established as the v2.2 pattern). No bundling step,
  no npm.
- File-size targets: explorer HTML approaching 1000 lines is fine
  with the new 3D panel; if it pushes much past, split JS into a
  separate `docs/explorer/explorer.js` served by the static server.

## When to session-close

If context fills past ~85% before all four ship: commit what's done,
update `.multi-session/session-last-ndb-architecture-foundation.md`
with progress + remaining items, push.

If A + B + C are done but D is mid-stream: that's a strong stopping
point — commit a "v2.2 explorer: AlphaFold + residue-level hypergraph"
artifact, leave D as the next session's primary work.

If all four ship: tag `v2.2.0` + create release. Update README's
"Status" section to mention the AlphaFold integration + folding
renderer.

## Helpful commands

```sh
# Run the current explorer (baseline)
cargo run -p ndb-renderer --example v22_explorer

# Verify pLDDT from AlphaFold-DB for one accession
curl -s https://alphafold.ebi.ac.uk/api/prediction/P04637 | jq '.[0].confidenceScore'

# Download a CIF for the folding viewer test
curl -s https://alphafold.ebi.ac.uk/files/AF-P04637-F1-model_v4.cif > /tmp/p53.cif
wc -l /tmp/p53.cif
```
