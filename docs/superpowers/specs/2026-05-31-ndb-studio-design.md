# nDB Studio — design spec

**Status:** approved design, pre-implementation
**Date:** 2026-05-31
**Author:** session design (Opus 4.8 + user)

## One line

A single installable binary that opens any nDB and lets users see it as
**familiar tables**, slice it into **2-D pivots**, **project it into a creative
3-D view they configure**, **create/edit data as versioned commits with
time-travel**, **query it in a live console**, and run it as a **multi-user,
replicated node** — with the nDB engine packed inside the same binary.

## Why this, and why now

nDB today is an engine plus six hand-built, read-only visual demos
(`docs/langgraph`, `biodiv`, `seismic`, `exoplanet`, `alphafold`, `chemistry`),
each hardcoded to one dataset. The data-shaping capability exists as libraries —
`ndb-slicer` (select-columns / filter / group-by / Count·Sum·Avg·Min·Max),
`ndb-renderer` (text tables), `ndb-query` (query language) — and the write path
exists via `ndb-cli`, `ndb-server`, `ndb-mcp-server`, plus engine-level
replication primitives. What does **not** exist is a single application that
fuses all of this over an *arbitrary* nDB: create/manage data, see it in
familiar table form, project it creatively, query it, and serve it to a team.

This spec defines that application — the most direct expression of the project
thesis: *the same n-dimensional object, projected to a familiar 2-D table and to
a creative view, with the user choosing the projection* — and adds the platform
layer (auth, replication, query console) that makes it a product, not a tool.

## Locked decisions (from brainstorming)

1. **Form factor — self-contained binary now, Tauri-portable later.** v1 is one
   Rust binary that links `ndb-engine` in-process and serves the web UI on
   localhost, auto-opening the browser. "Install" = drop one cross-platform
   binary. The frontend↔backend boundary is a single API surface so a later
   Tauri shell reuses the same frontend with no rewrite.
2. **Write model — edits are MVCC versions; time-travel is a feature.** Creating
   a record or changing a property commits a *new* version; nothing is
   overwritten. The UI surfaces this: per-cell history, and a global time-slider
   that views the whole DB as-of any past commit. Delete = tombstone version.
3. **Creative view — generic projection picker.** The user maps schema
   properties to visual channels (X / Y / Z axes, color, size, time) to render a
   3-D scatter, plus an auto-built force-graph from the N-ary hyperedges. Works
   on any nDB. Reuses the existing `3d-force-graph` stack.
4. **Multi-user — login + flat roles over a single-writer engine.** Local
   accounts (viewer / editor / admin) stored as `meta` records; session auth at
   the API; every commit is attributed to its author as version metadata. The
   nDB engine is **single-writer by design** (`with_write_txn` serializes
   writes); multi-user means identity + roles + attribution over that, **not**
   concurrent multi-writer conflict resolution (explicitly out of scope).

Two decisions made without a question (flagged and accepted in review):

- **Frontend is vanilla JS reusing `3d-force-graph`** (no React).
- **Table view = record-kind browser + a pivot mode** (both via `ndb-slicer`).

## Shell (user-facing layout)

```
┌─ CATALOG ───┬─ MAIN ──────────────────────────────────── 👤 alice (editor) ┐
│ Kinds       │ [ Table ] [ Pivot ] [ Creative ] [ Query ]   ⏱ as-of  ⚙ Node │
│  ▸ Paper    │ ┌──────────────────────────────────────────────────────────┐ │
│  ▸ Author   │ │ title        │ year │ field   │ cited  │                  │ │
│  ▸ …        │ │ Attention…   │ 2017 │ ML      │ 91k  ✎ │ ← edit = version │ │
│ Properties  │ │ BERT…        │ 2018 │ NLP     │ 70k    │                  │ │
│  • title    │ └──────────────────────────────────────────────────────────┘ │
│  • vec[768] │  + New record        🕑 cell history                          │
│ [+ label]   │                                                              │
└─────────────┴──────────────────────────────────────────────────────────────┘
```

Four tabs swap the *same selection* between Table / Pivot / Creative / Query.
The **⏱ time-slider** reads the whole DB as-of any past commit. A **👤 user
badge** shows identity/role; **⚙ Node** (admin only) opens the replication
panel.

## Architecture — eight units, one swappable seam

Core units (1–5) deliver the single-user app; platform units (6–8) add the
multi-user/replicated/query layer. Each unit has one purpose, a defined
interface, and is testable on its own.

### 1. Engine host (Rust)

Owns the `SharedEngine`, opened low-memory (mmap) by default. Typed command API:

- `open(path) / create(path)`.
- `read(id, as_of: Option<Tx>)` — current or historical record.
- `snapshot_iter(as_of: Option<Tx>)` — stream records for slicing.
- `slice(SliceRequest)` — delegate to `ndb-slicer` (table + pivot).
- `commit(WriteOp, author)` — create / set-property / tombstone via
  `with_write_txn`, stamping the author.
- `history(id)` — version chain `(tx, author, version)`.
- `query(text)` — run an `ndb-query` program, return rows.
- `vector_search(prop, query, k, metric)`.

Reuses the engine's write-admission + auto-flush so a UI-driven bulk import
cannot OOM. Depends on: `ndb-engine`, `ndb-slicer`, `ndb-query`.

### 2. Catalog service (Rust)

Derives "what is in this DB" by scanning, because nDB stores records as
`(kind, property_id, value)` with no built-in human names. Produces: kinds with
counts; properties per kind with inferred type and cardinality; vector flags
(kNN-eligible) and hyperedge flags (graph-eligible).

**Human labels for kinds/properties are themselves nDB records** under a
reserved `meta` kind — self-describing, editable, versioned. Unlabeled →
`kind:<id>` / `prop:<id>`. First scan is bounded and cached to a `catalog.json`
sidecar; later opens reload and refresh incrementally. Depends on: Engine host.

### 3. HTTP / IPC API gateway (Rust)

The single frontend↔backend boundary — **this is what a later Tauri build
swaps** (HTTP fetch → `invoke`). Every request passes the **auth middleware**
(unit 6) which resolves the session and enforces the role. Endpoints:

| Endpoint | Min role | Purpose |
|---|---|---|
| `POST /login`, `POST /logout` | — | session in/out |
| `GET /me` | viewer | current identity + role |
| `GET /catalog` | viewer | kinds, properties, labels, type/flags |
| `POST /label` | editor | set/edit a kind or property label |
| `POST /slice` | viewer | table or pivot result |
| `GET /record/{id}?as_of=tx` | viewer | record, current or historical |
| `GET /record/{id}/history` | viewer | version chain `(tx, author, version)` |
| `POST /commit` | editor | create / set-property / tombstone → new tx |
| `POST /project` | viewer | creative-view nodes + edges |
| `POST /query` | viewer | run `ndb-query` program → rows |
| `POST /vector-search` | viewer | kNN over a vector property |
| `GET /users`, `POST /users`, `POST /users/{u}/role` | admin | user management |
| `GET /node`, `POST /node/leader`, `POST /node/follower`, `POST /node/promote` | admin | replication topology |
| `GET /metrics` | admin | engine stats (reuse existing) |

`POST /commit` → `WriteStalled` maps to HTTP 503 ("busy, retry"); missing
session → 401; insufficient role → 403. Depends on: Engine host, Catalog,
Identity, Node-admin.

### 4. Frontend SPA (web, vanilla JS)

Reuses the explorer JS + `3d-force-graph` + the knowledge-site query editor.
Views: **Catalog sidebar** (editable labels); **Table** (filter/sort/inline-edit
→ version, 🕑 history popover); **Pivot** (two axes + aggregate → cross-tab);
**Creative** (projection picker → 3-D scatter + auto force-graph, selection
synced with the table); **Query** (console + live result table that can *feed*
the other tabs); **New-record form**; **Login screen**; **Users panel** (admin);
**Node panel** (admin); **Time-slider**. The UI is role-aware: viewers see no
edit affordances, admins see Users + Node.

### 5. Transport client (frontend)

One module all views call — `fetch` now, swappable to Tauri `invoke` later, with
session token handling in one place. The frontend half of the Tauri seam.

### 6. Identity & session service (Rust)

Local accounts stored as `meta` records: username, Argon2 password hash, role
(viewer / editor / admin). Issues session tokens (HTTP-only cookie); the API
middleware resolves token → user → role and enforces the per-endpoint minimum
role. Edits are attributed by passing the resolved user into `commit(..,
author)`. First `--new` DB bootstraps one admin (created at init, must set
password on first login). Depends on: Engine host (accounts are records).

### 7. Node-admin service (Rust)

Wraps the engine's existing replication primitives (`serve_replication`,
`ingest_replicated`, `poll_once`, follower cursors). Exposes node role
(standalone / leader / follower), peer URL, and live lag/cursor; lets an admin
make this node a leader, attach it as a follower to a peer (the follower poll
loop authenticates to the peer's auth-gated `/replicate`), or promote a
follower. This is the "cloud" story: run one Studio as leader, others as
followers or as remote auth'd clients. Depends on: Engine host, Identity (admin
gate + peer auth).

### 8. Query service (Rust)

Thin wrapper over `ndb-query`: parse + run a program against the current (or
as-of-tx) snapshot, return rows in the same shape as `/slice` so the Query tab's
results reuse the table grid, and a query can be promoted to the active
selection feeding Table/Pivot/Creative. Depends on: Engine host, `ndb-query`.

## Data flow

1. `ndb-studio <db>` (or `--new <path>`) → Engine host opens/creates the
   `SharedEngine`; Identity bootstraps admin on `--new`.
2. Catalog scans (or reloads `catalog.json`).
3. Browser opens → **Login** → session established → `/me` → role-aware shell.
4. Table/Pivot call `/slice`; Creative calls `/project`; Query calls `/query`;
   record detail calls `/record/{id}`.
5. An edit `POST /commit` (author = session user) → new tx → active view
   refreshes; the version is attributed in history.
6. Time-slider re-issues the active view's query with `as_of=tx`.
7. Admin opens **Node** → configures leader/follower; a follower runs the poll
   loop against the leader's auth-gated `/replicate`, applying batches via
   `ingest_replicated`.

## Error handling

- **Unauthenticated / insufficient role:** 401 / 403 with a clear message; the
  UI routes 401 to the login screen, 403 to a "not permitted" toast.
- **Write stall:** `/commit` → 503 → "engine busy, retry"; no data loss.
- **Unlabeled schema:** show `kind:<id>` / `prop:<id>`; never block.
- **Large slice / query:** server-side row limit + pagination; the slicer
  streams, so the backend stays bounded.
- **kNN on a non-vector property:** disabled in the picker; `/vector-search`
  rejects with a clear message if called anyway.
- **Replication peer unreachable / cursor gap:** Node panel shows the error +
  last-good cursor; the follower retries with backoff; a true gap (non-archived
  WAL) surfaces as "re-seed required", not silent divergence.
- **Open failure / corrupt DB:** surface the engine error verbatim in a startup
  banner; do not half-open.

## Testing

**Rust (per unit):**

- Catalog scan over a fixture → expected kinds/properties/types/flags.
- `/slice` table and pivot correctness vs hand-computed output.
- commit → history → as-of round-trip, asserting author attribution per version.
- `/project` determinism: same mapping + DB → identical payload.
- Label round-trip via `/label` → reflected in `/catalog`.
- Auth: login issues a session; viewer is 403 on `/commit`; editor succeeds;
  admin-only routes 403 for non-admins; bad password fails.
- Query: a known `ndb-query` program returns the expected rows.
- Node-admin: standalone → leader → follower attach → promote transitions, and a
  leader→follower replication round-trip applies a committed batch (reuse the
  existing replication tests as the engine-level basis).

**Frontend (thin Playwright smoke):**

- Login → Table renders rows.
- Inline-edit a cell → a new version (with author) appears in the history
  popover.
- Switch to Creative → set a projection → nodes render.
- Run a query in the Query tab → results render; promote to Table.
- Move the time-slider → the table shows the earlier value.
- As a viewer, edit affordances are absent.

Fixtures: a tiny hand-built DB plus the `langgraph --synthetic` generator.
Deterministic seeds; no network except a loopback leader/follower pair for the
replication smoke.

## Build phases (one spec, staged delivery)

Core first (showable, single-user), then the platform layer. Auth (P5) precedes
replication UI (P7) because remote replication access is auth-gated; the query
console (P6) is independent.

- **P1 — familiar core:** Engine host + Catalog + API gateway (no-auth shim) +
  Table view + create/edit-as-versions + time-slider.
- **P2 — 2-D slice:** Pivot mode + per-cell history popover.
- **P3 — creative:** projection picker + force-graph + table↔graph selection.
- **P4 — packaging:** embed frontend assets into the binary (`ndb-studio <db>` /
  `--new`), auto-open browser, verify the Tauri seam compiles.
- **P5 — multi-user:** Identity service + session auth middleware + role gate +
  author attribution + Login & Users panels; bootstrap admin on `--new`.
- **P6 — query console:** Query service + Query tab + promote-to-selection.
- **P7 — replication UI:** Node-admin service + admin Node panel over the
  engine's leader/follower primitives, with auth-gated peer access.

## Reuse vs new

**Reuse:** `ndb-slicer` (tables + pivot), `ndb-query` (query console), low-memory
`ndb-engine` + its replication primitives, `ndb-server` HTTP + write-admission
patterns, `3d-force-graph` + explorer JS, the knowledge-site query editor,
`/metrics`.

**New:** catalog service, `/project` endpoint, identity/session service, the
node-admin service, query-to-views wiring, the frontend shell, the
embed/launcher binary, the swappable transport client.

## Out of scope (YAGNI)

- **Concurrent multi-writer** conflict resolution — the engine is single-writer
  by design; this would be a major engine redesign. Multi-user is identity +
  roles + attribution only.
- **External identity providers (OIDC/SAML)** — local accounts only in this
  spec; an IdP integration is a later, separate change.
- **Hosted SaaS / billing / org-tenancy** — "cloud" here means self-hosted
  leader/follower nodes, not a managed service.
- The six bespoke demos stay as-is showcases; this app does not replace them.

## Open questions deferred to implementation planning

- Exact wire shape of `SliceRequest` / `/project` / `/query` payloads (settled
  against the real `ndb-slicer` and `ndb-query` APIs).
- Where author attribution is stored: a per-version property vs a parallel audit
  record (decided against the engine's tx metadata capabilities).
- Code location: leaning `crates/ndb-studio` (links engine + slicer + query like
  other first-class crates) over `tools/`.
- Catalog cache invalidation on incremental writes (full vs delta scan).
- Session token storage and expiry policy (in-DB `meta` sessions vs in-memory).
