# nDB Studio

A single in-process binary that opens any nDB and serves a local web app to
**manage, view, edit, query, explore, and replicate** it — with the engine
linked in (one process is the whole application). It's built to surface what
makes nDB different from a relational database, not to be a generic table admin.

```
cargo run -p ndb-studio -- --new /path/to/db     # create + open
cargo run -p ndb-studio -- /path/to/db           # open existing
```

Flags: `--bind <addr>` (default `127.0.0.1:0`), `--low-memory` (mmap / bounded
RAM), `--no-open` (don't launch a browser). On first run against a database
with no accounts, a bootstrap **admin** is created and its one-time password is
printed to the console.

## What it does

**Familiar core**
- **Table** view per record-kind with per-column filters, global search,
  click-to-sort headers, pagination, multi-select **bulk delete** and **bulk
  set-field** (which doubles as add-column, since nDB is schema-on-read).
- **Create / edit as versions** — every edit is a new MVCC version; nothing is
  overwritten.
- **Export / import** CSV + JSON.

**The nDB differentiators** (no relational equivalent)
- **N-ary hyperedge workbench** — one edge spans N entities by named **role**
  (where SQL needs junction tables). Edges carry their own **properties** and
  can fill roles in **other edges** (edges-on-edges).
- **360° entity view** — one panel unifying an entity's properties,
  relationships (refs in/out), hyperedge roles, version count, and vector.
- **Time-travel** — a slider reads the whole DB as of any transaction, with an
  **as-of wall-clock** picker; per-cell **history** popovers; a **temporal
  diff** of any two points (added / changed old→new / removed).
- **Graph** — a self-contained 2-D force layout; **ego-graph exploration**
  (expand from a node hop-by-hop) and **▶ play history** (watch the graph grow
  as commits land).
- **Vector find-similar** — cosine kNN over a vector property; semantic search
  inside the database, no separate vector store.

**Query & platform**
- **Query console** (the `ndb-query` language), read-only, with history + saved
  queries; results promote to the record view.
- **Schema overview** and **integrity checks** (dangling refs / bad edge
  fillers).
- **Multi-user** — local accounts, cookie sessions, roles
  (viewer / editor / admin), author attribution.
- **Multi-database** — create / open / switch databases in-app (the launch DB
  holds the accounts).
- **Replication** — leader/follower over the engine's WAL stream + a node panel
  showing the live **storage-engine internals** (memtable, SSTables, index RAM,
  MVCC snapshots) and a Compact button.
- **Command palette** (⌘K / Ctrl+K) and an in-app **Concepts** guide that maps
  every nDB term to its SQL analogue with a try-it example.

## Architecture

One swappable frontend↔backend seam, so a later Tauri shell can replace `fetch`
with `invoke` against the same routes.

| Module | Responsibility |
|---|---|
| `store` | The **only** code that touches `ndb-engine`. Derives the catalog by scanning, projects record-kinds to tables/pivots/graphs, turns create/edit/delete into MVCC commits, and exposes history/diff/vector/replication/engine-stats. Everything above it speaks `serde_json`. |
| `http` | Hand-rolled HTTP/1.1 server + the single embedded web UI (`web/index.html`, vanilla JS). Cookie auth, role middleware, multi-database routing, and a minimal HTTP client for replication pulls. |
| `identity` | Pure password hashing (salted, iterated SHA-256) + in-memory sessions + roles. |
| `jsonval` | `Value` ⇆ JSON. Scalars, `{"$ref":uuid}` (edges), `{"$vec":[…]}` (vectors). |

Reserved kinds/properties (names starting with `$`, e.g. `$User` accounts and
`$author` attribution) are filtered out of every data view.

## API (all JSON; auth via the `ndb_session` cookie)

Public: `GET /api/health`, `GET /api/me`, `POST /api/login`, `POST /api/logout`.

Reads (any role): `GET /api/catalog | table | record | history | pivot | graph
| neighbors | hyperedges | schema | integrity | diff | similar | tx_time |
tx_at`; `POST /api/query`.

Writes (editor/admin): `POST /api/commit` (`op`: `create` / `set` / `delete` /
`create_edge` / `register_vector`).

Admin: `GET|POST /api/users…`, `…/databases…`, `…/replication…`, `GET
/api/engine/stats`, `POST /api/engine/compact`.

## Status

All phases of the design spec
(`docs/superpowers/specs/2026-05-31-ndb-studio-design.md`) plus the extension
work are implemented and covered by store/identity unit tests; the one engine
addition (`versions_of`) ships with its own test and leaves the full engine
suite green. The active multi-database selection is process-global (a
single-operator desktop tool).
