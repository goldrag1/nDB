# nDB Studio — design spec

**Status:** approved design, pre-implementation
**Date:** 2026-05-31
**Author:** session design (Opus 4.8 + user)

## One line

A single installable binary that opens any nDB and lets a user see it as
**familiar tables**, slice it into **2-D pivots**, **project it into a creative
3-D view they configure**, and **create/edit data as versioned commits with
time-travel** — with the nDB engine packed inside the same binary.

## Why this, and why now

nDB today is an engine plus six hand-built, read-only visual demos
(`docs/langgraph`, `biodiv`, `seismic`, `exoplanet`, `alphafold`, `chemistry`).
Each is hardcoded to one dataset. The data-shaping capability exists as
libraries — `ndb-slicer` (select-columns / filter / group-by / Count·Sum·Avg·Min·Max)
and `ndb-renderer` (text tables) — and the write path exists via `ndb-cli`,
`ndb-server`, and `ndb-mcp-server`. What does **not** exist is a single
application that fuses all three over an *arbitrary* nDB: create/manage data,
see it in familiar table form, and project it into creative views.

This spec defines that application. It is the most direct expression of the
project thesis: *the same n-dimensional object, projected to a familiar 2-D
table and to a creative view, with the user choosing the projection.*

## Locked decisions (from brainstorming)

1. **Form factor — self-contained binary now, Tauri-portable later.** v1 is one
   Rust binary that links `ndb-engine` in-process and serves the web UI on
   localhost, auto-opening the browser. "Install" = drop one cross-platform
   binary. The frontend↔backend boundary is a single API surface so a later
   Tauri shell reuses the same frontend with no rewrite.
2. **Write model — edits are MVCC versions; time-travel is a feature.** Creating
   a record or changing a property commits a *new* version; nothing is
   overwritten. The UI surfaces this: per-cell history, and a global time-slider
   that views the whole DB as-of any past commit. Delete = tombstone version
   (stays in history).
3. **Creative view — generic projection picker.** The user maps schema
   properties to visual channels (X / Y / Z axes, color, size, time) to render a
   3-D scatter, plus an auto-built force-graph from the N-ary hyperedges. Works
   on any nDB because the user drives the mapping. Reuses the existing
   `3d-force-graph` stack.

Two decisions made without a question (flagged and accepted in review):

- **Frontend is vanilla JS reusing `3d-force-graph`** (no React) — lighter,
  Tauri-friendly, consistent with the existing explorer code.
- **Table view = record-kind browser + a pivot mode.** Both are computed by
  `ndb-slicer` already.

## Shell (user-facing layout)

```
┌─ CATALOG ───┬─ MAIN ─────────────────────────────────────┐
│ Kinds       │ [ Table ] [ Pivot ] [ Creative ]   ⏱ as-of │
│  ▸ Paper    │ ┌────────────────────────────────────────┐ │
│  ▸ Author   │ │ title        │ year │ field   │ cited  │ │
│  ▸ …        │ │ Attention…   │ 2017 │ ML      │ 91k  ✎ │ │ ← inline edit = new version
│ Properties  │ │ BERT…        │ 2018 │ NLP     │ 70k    │ │
│  • title    │ └────────────────────────────────────────┘ │
│  • vec[768] │  + New record        🕑 cell history         │
│ [+ label]   │                                             │
└─────────────┴─────────────────────────────────────────────┘
```

The three tabs swap the *same selection* between Table / Pivot / Creative. The
**⏱ time-slider** reads the whole DB as-of any past commit.

## Architecture — five units, one swappable seam

Each unit has one purpose, a defined interface, and is testable on its own.

### 1. Engine host (Rust)

Owns the `SharedEngine`, opened in low-memory (mmap) mode by default. Exposes a
typed command API the HTTP layer calls:

- `open(path) / create(path)` — open or initialize a DB.
- `read(id, as_of: Option<Tx>)` — current or historical record.
- `snapshot_iter(as_of: Option<Tx>)` — stream records for slicing.
- `slice(SliceRequest)` — delegate to `ndb-slicer` (table + pivot).
- `commit(WriteOp)` — create / set-property / tombstone via `with_write_txn`.
- `history(id)` — the version chain for a record (list of `(tx, version)`).
- `vector_search(prop, query, k, metric)` — kNN over a vector property.

Reuses the engine's existing write-admission + auto-flush so a bulk import
through the UI cannot OOM the process. Depends on: `ndb-engine`, `ndb-slicer`.

### 2. Catalog service (Rust)

Derives "what is in this DB" by scanning, because nDB stores records as
`(kind, property_id, value)` with no built-in human names. Produces:

- record-kinds present, with counts;
- property-ids per kind, with inferred value type (I64 / F64 / Decimal /
  String / Bool / Timestamp / Vector / hyperedge-ref) and cardinality;
- which properties are vectors (eligible for the kNN channel) and which
  participate in hyperedges (eligible for the force-graph).

**Human labels for kinds and properties are themselves nDB records** under a
reserved `meta` kind, so the catalog is self-describing and editable from the
UI. When a kind/property has no label, the UI shows `kind:<id>` / `prop:<id>`.

The first scan of a large DB is bounded and cached to disk (a `catalog.json`
sidecar next to the DB); subsequent opens reload it and refresh incrementally.
Depends on: Engine host.

### 3. HTTP / IPC API (Rust)

The single boundary between frontend and backend — **this is what a later Tauri
build swaps** (HTTP fetch → `invoke`). Endpoints:

| Endpoint | Purpose |
|---|---|
| `GET /catalog` | kinds, properties, labels, type/cardinality, vector & edge flags |
| `POST /label` | set/edit a kind or property label (writes a `meta` record) |
| `POST /slice` | table or pivot result (`SliceRequest` → rows or cross-tab) |
| `GET /record/{id}?as_of=tx` | one record, current or historical |
| `GET /record/{id}/history` | version chain `(tx, version)` |
| `POST /commit` | create / set-property / tombstone → returns new tx |
| `POST /project` | creative-view payload: nodes (positioned from property→channel mapping) + edges (from hyperedges) |
| `POST /vector-search` | kNN over a vector property |
| `GET /metrics` | engine index/memtable/sstable stats (reuse existing) |

`POST /commit` returning `WriteStalled` (engine under pressure) maps to HTTP 503
so the frontend can show a "busy, retry" toast. Depends on: Engine host,
Catalog service. Reuses `ndb-server` HTTP + admission patterns.

### 4. Frontend SPA (web, vanilla JS)

Reuses the existing explorer JS + `3d-force-graph`. Views:

- **Catalog sidebar** — kinds and properties; inline-editable labels.
- **Table view** — pick a kind → grid (rows = records, cols = properties), with
  filter and sort; inline cell edit → `POST /commit` (new version); a per-cell
  history popover (🕑) reading `/record/{id}/history`.
- **Pivot view** — pick two properties as axes + an aggregate → cross-tab table
  (the 2-D slice of the N-D space). Computed by `/slice`.
- **Creative view** — projection picker mapping properties → X/Y/Z/color/size/
  time channels → 3-D scatter; plus an auto-built force-graph from hyperedges.
  Selecting a node highlights the matching table row and vice versa.
- **New-record form** — fields derived from the catalog for the chosen kind.
- **Time-slider (⏱)** — global as-of-tx control; re-queries the active view.

### 5. Transport client (frontend)

One small module that all views call. Implements the API over `fetch` now; later
re-points to Tauri `invoke` with no change to the views. This is the frontend
half of the Tauri seam.

## Data flow

1. `ndb-studio <db>` (or `--new <path>`) → Engine host opens/creates the
   `SharedEngine`.
2. Catalog service scans (or reloads `catalog.json`) → catalog ready.
3. Browser opens → frontend loads `/catalog` → Catalog sidebar renders.
4. Table/Pivot views call `/slice`; Creative view calls `/project`; record
   detail calls `/record/{id}`.
5. An edit `POST /commit` → new tx → the active view refreshes from the new
   snapshot.
6. Moving the time-slider re-issues the active view's query with `as_of=tx`.

## Error handling

- **Write stall:** `/commit` → 503 → "engine busy, retry" toast; no data loss.
- **Unlabeled schema:** show `kind:<id>` / `prop:<id>`; never block on missing
  labels.
- **Large slice:** server-side row limit + pagination; `ndb-slicer` already
  streams, so the backend stays bounded.
- **kNN on a non-vector property:** the channel/option is disabled in the picker;
  `/vector-search` rejects with a clear message if called anyway.
- **Open failure / corrupt DB:** surface the engine error verbatim in a startup
  banner; do not half-open.

## Testing

**Rust (per unit):**

- Catalog scan over a known fixture DB → expected kinds/properties/types/flags.
- `/slice` table and pivot correctness against hand-computed expected output.
- commit → history → as-of round-trip: write three versions, assert history
  length and that `read(id, as_of=tx_n)` returns version n.
- `/project` determinism: same mapping + same DB → identical node/edge payload.
- Label round-trip: `POST /label` writes a `meta` record; `/catalog` reflects it.

**Frontend (thin Playwright smoke):**

- Open a small synthetic fixture DB → Table renders rows.
- Inline-edit a cell → a new version appears in the cell-history popover.
- Switch to Creative → set a projection → nodes render.
- Move the time-slider → the table shows the earlier value.

Fixtures: a tiny hand-built DB plus the existing `langgraph --synthetic`
generator for a larger smoke. Deterministic seeds; no network.

## Build phases (one spec, staged delivery)

- **P1 — familiar core:** Engine host + Catalog + HTTP API + Table view +
  create/edit-as-versions + time-slider. Delivers "create + manage as tables."
- **P2 — 2-D slice:** Pivot mode + per-cell history popover.
- **P3 — creative:** projection picker + force-graph + table↔graph selection
  sync. Delivers the thesis.
- **P4 — packaging:** embed frontend assets into the binary (`ndb-studio <db>`
  / `--new`), auto-open browser, verify the Tauri seam compiles against the same
  frontend.

## Reuse vs new

**Reuse:** `ndb-slicer` (tables + pivot), low-memory `ndb-engine`, `ndb-server`
HTTP + write-admission patterns, `3d-force-graph` + explorer JS, `/metrics`.

**New:** the catalog service, the `/project` endpoint, the frontend shell, the
embed/launcher binary, and the swappable transport client.

## Out of scope (YAGNI)

- No auth / multi-user — local, single-user.
- No cloud, no replication UI (replication exists in the engine; not surfaced
  here).
- Not a rewrite of the query-language editor — link to the existing one in
  `knowledge-site`.
- The six bespoke demos stay as-is showcases; this app does not replace them.

## Open questions deferred to implementation planning

- Exact wire shape of `SliceRequest` / `/project` payloads (settled in the plan
  against the real `ndb-slicer` API).
- Where the new code lives: a new `crates/ndb-studio` binary vs `tools/ndb-studio`
  (decided in the plan; leaning `crates/` since it links engine + slicer like
  other first-class crates).
- Catalog cache invalidation policy on incremental writes (full vs delta scan).
