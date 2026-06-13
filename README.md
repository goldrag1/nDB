# nDB

An n-dimensional hypergraph database engine in Rust.

This repository contains the v1 storage core and companion crates. nDB is
**hyperedge-native**: the atomic unit is an N-ary fact connecting any number of
entities in named role slots — not a row in a table, and not a binary edge with
reified properties. See the [white paper](docs/nDB-whitepaper.md) for the
architectural narrative and the [full design spec](docs/superpowers/specs/2026-05-27-nDB-hypergraph-design.md)
for the byte-level details.

## Install & Quickstart

Released as **v2.4.0** — installable, not just source.

**Use it from TypeScript** (Node, browser, Deno, edge — zero deps):

```sh
npm i @n-dimension-database-ndb/client
```
```ts
import { NdbClient } from "@n-dimension-database-ndb/client";
const db = new NdbClient("http://127.0.0.1:8742");
console.log((await db.health()).status); // "ok"
```

**Run the server** (Docker):

```sh
docker run -p 8742:8742 -v ndb-data:/data ghcr.io/goldrag1/ndb
curl localhost:8742/v1/health            # {"status":"ok"}
```

…or grab a prebuilt binary (linux x86_64 / aarch64, macOS arm64) from the
[latest release](https://github.com/goldrag1/nDB/releases/latest), or
`cargo run --release -p ndb-server -- --path ./db`.

**Point an AI agent at it** (Model Context Protocol — Claude / Cursor / Codex):

```sh
npx @n-dimension-database-ndb/mcp --path ./db
```

Full walkthrough: **[QUICKSTART](docs/QUICKSTART.md)** · wire contract:
**[PROTOCOL](docs/PROTOCOL.md)** · "your data always opens" upgrade guarantee:
**[COMPATIBILITY](docs/COMPATIBILITY.md)** · deploy (compose, Helm, sharded
cluster + replication): **[deploy/README](deploy/README.md)**.

## Workspace

| Crate                       | Purpose                                                                              |
|-----------------------------|--------------------------------------------------------------------------------------|
| `ndb-engine`                | Storage core: records, WAL, mmap-backed SSTable, MANIFEST, memtable, MVCC + SSI, compaction with per-type retention, indexes, metadata-driven validation, query planner/executor, AES-GCM-256 at-rest encryption primitives. Library only. |
| `ndb-query`                 | Text → name-AST → id-AST (`QueryRequest`) for the §12 query language. Lexer + recursive-descent parser + dictionary resolver. Library only. |
| `ndb-server`                | Hand-rolled HTTP/1.1 server exposing the engine over JSON. TLS via rustls; ReBAC capability gating; .audit.jsonl. Binary `ndb-server`. |
| `ndb-client-rust`           | Reusable Rust client library (`ndb_client::Client`) — typed against the engine's wire shapes. |
| `ndb-cli`                   | Command-line client over `ndb-client-rust`. Binary `ndb`.                            |
| `ndb-mcp-server`            | Model Context Protocol bridge — exposes the engine to AI agents via stdio JSON-RPC, with the same ReBAC + audit-log surface as `ndb-server`. Binary `ndb-mcp-server`. |
| `ndb-router`                | Stateless sharding coordinator — fans `/v1` across N single-writer shards: hash routing, hyperedge anchor placement, scatter-on-miss reads, kNN top-k merge, cross-shard traverse. Binary `ndb-router`. |
| `ndb-slicer`                | CPU projection + aggregation over `Engine::snapshot_iter` output.                    |
| `ndb-renderer`              | Text/TSV/CSV table output for `ndb-slicer` results.                                  |
| `ndb-arrow`                 | Apache Arrow IPC bridge — `Engine::snapshot_iter` → `RecordBatch` + IPC bytes for Polars / pandas / DuckDB consumers. |
| `ndb-index-vector-hnsw`     | HNSW ANN vector index (opt-in plugin) — drop-in replacement for the brute-force baseline once dataset size warrants it. |
| `clients/python/ndb_client` | Pure-Python (`urllib`-only) HTTP client. `pip install ndb-client`.                  |
| `clients/ts`                | Zero-dep typed TypeScript SDK. `npm i @n-dimension-database-ndb/client`. Node / browser / Deno / edge. |
| `clients/mcp-npm`           | `npx`-runnable MCP launcher. `npx @n-dimension-database-ndb/mcp --path ./db`. Resolves the per-platform server binary. |

## For structural biologists — start here

If you arrived because you care about proteins more than databases,
the v2.2 explorer ships a working 3D demo plus a science-friendly
landing page at **[`docs/alphafold_nDB/README.md`](docs/alphafold_nDB/README.md)**.
30-second tour:

```sh
cargo run -p ndb-renderer --example v22_explorer
# then open http://127.0.0.1:9876/
```

Mean pLDDT from AlphaFold-DB drives node colour + size. Click a
protein → "Load AlphaFold 3D structure" → cartoon coloured by per-
residue confidence appears in the right pane. Toggle "Show residues"
to surface 78 residue entities + 8 structural-motif hyperedges
(catalytic triad, zinc finger, disulfides, α-helix, β-sheet pair).
Clicking a motif hyperedge → its members glow in 3D. Floating "What
does nDB store for this protein?" pane explains the data shape in
plain language with live wire-format JSON. Full provenance and
reproducibility steps at
[`docs/alphafold_nDB/REPRODUCIBILITY.md`](docs/alphafold_nDB/REPRODUCIBILITY.md).

## Status (v2.3)

v2.3 turns alphafold_nDB into a real structural-biology tool with
honest provenance, not a metadata-only demo:

- **Engine arity bumped u8 → u32** (`record.rs` FORMAT_VERSION 1 → 2).
  Hyperedge arity is no longer capped at 255 — `protein_atoms` for
  KRAS is one arity-1518 record. Without this, the "N-dimensional"
  pitch was structurally bounded at 255.
- **Atom-level entities for every protein** — every CIF atom is a
  first-class indexed nDB entity (`type 7`) with x/y/z/B-factor/
  element/residue properties. Per-atom queries hit the property
  B-tree directly. For KRAS, 1517 atom entities + 1 arity-1518
  `protein_atoms` hyperedge.
- **No CIF blob duplication.** Earlier iterations stored both the
  CIF text AND atom entities — 3× the atom data. Path B fix: the
  CIF is never persisted; on subsequent loads a minimal PDB is
  rebuilt in memory from atom entities and handed to NGL. Single
  source of truth = atom entities in nDB.
- **N-ary "contains" replaces N reified binary edges.** Old design
  was 1517 atom_of edges; new design is 1 `protein_atoms` edge with
  1518 role-fillers. Same fix for `protein_residues` (5 N-ary edges
  instead of 78 binary). This is the model nDB was built for; the
  binary-edge anti-pattern was the bug.
- **Persistent DB across runs.** `/tmp/v22-explorer-ndb` no longer
  wiped on boot. First run seeds the curated 20 proteins; subsequent
  runs reuse the existing store, atoms and all.
- **Three-pane layout** (top bar + left "Protein in nDB" + 3D centre
  + right "Drill down") with live atom-click → residue card.
- **Side-by-side PDBx-vs-nDB comparison** with honest per-protein
  vs engine/schema KPI split — measured numbers only, fabricated
  benchmarks explicitly marked "not measured".

See [`docs/alphafold_nDB/`](docs/alphafold_nDB/) for the science-
facing landing page and reproducibility cheatsheet.

## Status (v2.2)

Every line item in the v1 spec §17.1 PLUS every deliverable from the v2.0
+ v2.1 + v2.2 working specs is shipped. **487 Rust tests + 12 Python
tests, clippy clean with `-D warnings`** as of this writing.

What's new in v2.2 (Nobel-themed AlphaFold integration):

- **AlphaFold pLDDT confidence overlay** — real `globalMetricValue`
  fetched from `alphafold.ebi.ac.uk/api/prediction/<acc>` for 18
  proteins (2 retired by AF-DB for >2700 aa proteins). Node colour +
  size driven by the AF-DB palette + thresholds. Stacked confidence
  distribution bar in the sidebar.
- **Live AlphaFold-DB fetch** from the sidebar — paste any UniProt
  accession; the protein lands in the database with real pLDDT
  bucket, gene, organism, and sequence length.
- **Residue-level hypergraph** for trypsin, TFIIIA, insulin,
  myoglobin, GFP — 78 residue entities + 8 N-ary motif hyperedges
  (catalytic_triad arity 3, zinc_finger arity 4, alpha_helix arity
  16, beta_sheet_pair arity 20, disulfide_bond arity 2). The case
  where binary edges have to invent a dummy node and nDB just
  stores the relationship directly.
- **NGL Viewer in the right pane** — AlphaFold cartoon coloured by
  per-residue pLDDT in the canonical AF palette. Bidirectional
  click sync: click a motif hyperedge → its members glow as
  ball+stick in the 3D structure; click a residue in the canvas →
  jump back to its nDB entity in the hypergraph.
- **nDB model explainer pane** — floats over the 3D view; shows
  the actual entity + hyperedge records nDB holds for the currently-
  loaded protein with live wire-format JSON. Designed so a
  structural biologist who's never seen a hyperedge database can
  understand the data shape in 30 seconds.

See `docs/alphafold_nDB/README.md` for the science-facing tour and
`docs/alphafold_nDB/REPRODUCIBILITY.md` for the verification cheatsheet.

What's shipped (v2.0 polish on top of v1):

- **Block index sidecar** (`<seq>.idx`) — O(log N) SSTable lookups via mmap-loaded sorted index
- **Persisted commit timestamps + retention policies** — survive engine restart (new record kinds 0x07 / 0x08)
- **Engine-side lazy iterator pipeline** — true streaming `/iter` + `/query_stream` via k-way-merge across memtable + SSTables
- **SharedEngine** — `Arc`-wrappable, concurrent writers serialize internally via `Mutex<Engine>`
- **Snapshot-aware compaction** — refcounted active-snapshot registry; in-flight readers never lose their versions
- **Cardinality-aware query planner** — greedy smallest-seed + shared-vars-first ordering; `Engine::explain_query` for EXPLAIN traces
- **Condvar-based `/subscribe` + thread-per-connection accept loop** — sub-millisecond wake latency on commit
- **WAL + SSTable AES-GCM-256 at-rest encryption** — `.encryption` marker file + `Engine::create_with_cipher` / `::open_with_cipher`
- **Capability hyperedges** — persistent ReBAC store via reserved type/role/property IDs; bootstrap import from `.principals.json`

What's shipped from v1:

- Append-only LSM storage (records, WAL `.ndblog`, SSTable `.ndb`, MANIFEST, CURRENT, LOCK)
- **Mmap'd SSTable reads** (`memmap2` — cold-read fast path)
- MVCC with snapshot reads + supersession derived at read time
- Single-writer transaction commit with validation pre-check
- **Snapshot Isolation + Serializable Snapshot Isolation** (per-txn opt-in via `WriteTxn::with_isolation`; read-set tracking + commit-time conflict detection)
- **Per-type retention policies** — `LatestOnly` (default), `Versioned { keep_last_n }`, `Audited`
- All 6 mandatory v1 indexes (entity-by-id, hyperedge-by-id, lookup-key, adjacency, hyperedge-type-cluster, property B-tree)
- Brute-force vector index (k-NN, L2 / cosine) + opt-in HNSW (`ndb-index-vector-hnsw`)
- Full compaction with cross-bucket tombstone handling + per-type retention
- **Validation engine + metadata-hyperedge-driven constraints** (constraints live in the database; loaded at `Engine::open`)
- JSON wire protocol over HTTP/1.1
- **Query language §12 end-to-end** — text → name AST (`ndb-query`) → wire AST (id-based) → planner → executor → `POST /query`. SQL-ish surface, n-ary pattern matching, recursive paths (`*`, `+`, `?`, `{n,m}`), where-clause filters, time travel (`as of <tx_id>` or `<timestamp>`), limit. Typed clients in Rust, Python, and CLI.
- **Streaming variants** — `POST /query_stream` emits JSONL line-by-line; `POST /subscribe` long-polls for newly-committed records
- **Time travel** via `?snapshot=N` and `?timestamp_us=T` on `/read` and `/iter` (commit timestamps recorded in-memory per session)
- Security baseline:
  - Bearer-token auth + multi-principal ReBAC (capability set per token)
  - TLS termination via rustls (`--tls-cert` / `--tls-key`)
  - `.audit.jsonl` per request — shared between HTTP server and MCP server
  - At-rest encryption primitives (`Cipher`, `EncryptedFile`) ready for WAL/SSTable wiring
- CLI client over HTTP (`ndb`) with `query` subcommand
- MCP server over stdio JSON-RPC, principal- and audit-aware
- CPU slicer (project, filter, group-by, sum/avg/count/min/max, sort, limit)
- Text/TSV/CSV renderer
- Apache Arrow IPC bridge (`ndb-arrow`)
- Pure-Python HTTP client (`clients/python/ndb_client`) with `query()` method

What's explicitly deferred (v2.1+ / v3):

- `Engine::reencrypt(new_key)` for key rotation + plaintext↔encrypted migration
- Server auth dispatch via `Engine::has_capability` instead of the in-memory cache (bootstrap import already lands)
- IVF / ScaNN vector indexes alongside HNSW
- True multi-writer / distributed mode (SSI API surface is ready, semantics no-op in single-process)
- Distributed mode + geo-replication (v3+)
- Write-via-query (extending §12 grammar with mutations)
- gRPC alternative transport
- JS/TS and Go client crates

## Quick start

```sh
# Build everything.
cargo build

# Run the in-process example (best for first-look).
cargo run -p ndb-engine --example basic

# Stand up the HTTP server in one terminal.
cargo run -p ndb-server -- --path /tmp/mydb

# Hit it from another terminal.
cargo run -p ndb-cli -- health
cargo run -p ndb-cli -- iter

# Or talk to it as an AI agent via stdio JSON-RPC.
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | \
  cargo run -p ndb-mcp-server -- --path /tmp/mydb
```

With a bearer token:

```sh
NDB_TOKEN=mysecret cargo run -p ndb-server -- --path /tmp/mydb &
NDB_TOKEN=mysecret cargo run -p ndb-cli -- iter
```

## On-disk layout

```
mydb/
├── CURRENT              # text pointer to active MANIFEST
├── LOCK                 # exclusive file lock
├── MANIFEST-000042      # versioned snapshot of active SSTables + watermarks
├── 000001.ndblog        # active WAL (will rotate on flush)
├── 000002.ndb           # SSTable, level 0 / level 1
└── 000003.ndb           # ...
```

Every record on disk carries `record_size`, `record_kind`, `format_version`,
type-specific payload, and CRC32. Records are sorted by `(record_kind,
primary_id)` inside SSTables. Atomic publish via `write-temp + fsync + rename
+ fsync_dir`.

## Wire protocol

Routes exposed by `ndb-server`:

| Method | Path                         | Body                                | Response                                              |
|--------|------------------------------|-------------------------------------|-------------------------------------------------------|
| GET    | `/health`                    | —                                   | `{"status":"ok"}`                                     |
| POST   | `/commit`                    | `CommitRequest { records: [...] }`  | `CommitResponse { tx_id }`                            |
| GET    | `/read/:uuid?snapshot=N`     | —                                   | `ReadResponse { outcome: missing\|deleted\|live, ... }` |
| GET    | `/iter?timestamp_us=T`       | —                                   | JSONL stream of records (live at the snapshot)        |
| POST   | `/lookup`                    | `LookupRequest`                     | `LookupResponse`                                      |
| POST   | `/vector_search`             | `VectorSearchRequest`               | `VectorSearchResponse`                                |
| POST   | `/property_lookup`           | `PropertyLookupRequest`             | `PropertyLookupResponse`                              |
| POST   | `/property_range`            | `PropertyRangeRequest`              | `PropertyRangeResponse`                               |
| POST   | `/traverse`                  | `TraverseRequest`                   | `TraverseResponse`                                    |
| POST   | `/query`                     | `QueryRequest`                      | `QueryResponse`                                       |
| POST   | `/query_stream`              | `QueryRequest`                      | JSONL: header + one row per line                      |
| POST   | `/subscribe`                 | `SubscribeRequest`                  | JSONL: header + one record per line                   |
| POST   | `/flush`                     | —                                   | memtable + SSTable counts                             |
| POST   | `/compact`                   | —                                   | `CompactionStats`                                     |

`/read` and `/iter` accept `?snapshot=<tx_id>` OR `?timestamp_us=<T>` to
pin the snapshot. Default is the engine's latest committed transaction.

JSON shapes use tagged-union for `Value` and per-kind discriminator for
records. Wire format details live in `ndb-engine::wire`.

## Repository layout

```
nDB-ndimemsion-database/
├── Cargo.toml                  # workspace
├── rust-toolchain.toml         # stable, edition 2024
├── docs/
│   ├── nDB-whitepaper.md       # public-facing narrative
│   └── superpowers/specs/...   # full architectural design + decision log
├── crates/
│   ├── ndb-engine/             # the storage core (library)
│   ├── ndb-server/             # HTTP server (library + binary)
│   ├── ndb-cli/                # command-line client (binary)
│   ├── ndb-mcp-server/         # MCP bridge (library + binary)
│   ├── ndb-slicer/             # projection + aggregation (library)
│   └── ndb-renderer/           # text/TSV/CSV output (library)
└── .multi-session/             # session-handoff notes for the build process
```

## License

Apache-2.0. See workspace `Cargo.toml`.
