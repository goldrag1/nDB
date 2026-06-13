# nDB Codebase Map

nDB is an n-dimensional hypergraph database with an append-only LSM storage core: entities + N-ary hyperedges, a self-describing tagged `Value` union, MVCC snapshots (time-travel), property/vector/adjacency indexes, and a CPU/GPU slicer for projection + aggregation. Atoms of data are stored n-dimensionally and projected to 2D tables, 3D force layouts, time scrubbers, and semantic (vector) views.

**Layering:** `ndb-engine` (storage + query core, in-process) → `ndb-server` / `ndb-router` (HTTP wire bridge + sharding) → `ndb-client-rust` / TS SDK / `ndb-mcp-server` (clients & agent bridge) → `ndb-slicer` / `ndb-renderer` / explorer (projection & visualization). `nstack` is a typed ERP kernel on the engine; `ndb-studio` is a standalone table/projection/edit app.

Workspace: edition 2024, `unsafe_code = forbid`, version 1.3.0. Authoritative design specs live in `docs/superpowers/specs/`.

## Crates (14 + 1 tool)

| Crate | Purpose (key files / entry points) |
|---|---|
| `ndb-engine` | Append-only LSM storage core — §11 record formats, tagged `Value`, MVCC, indexes, query exec. Entry: `Engine`, `snapshot_iter`, `QueryRequest`, `VectorIndex`. Files: `engine.rs`, `db.rs`, `record.rs`, `value.rs`, `id.rs`, `memtable.rs`, `sstable.rs`, `wal.rs`, `mvcc.rs`, `wire.rs`/`wire_query.rs`, `replication.rs`, `encryption.rs`, `compression.rs`, `backup_archive.rs`, `index/` (property_btree, vector, adjacency, type_cluster, *_file.rs mmap variants), `query/` (plan, mod). |
| `ndb-server` | HTTP/1.1 wire bridge to `Engine` — hand-rolled `std::net`, single-threaded, engine in a `Mutex`. Routes: `/health`, `/commit`, `/read/:uuid`, `/iter` (JSONL), `/arrow/export`, `/arrow/vectors`, `/arrow/edge_index`. Bin `main.rs` + `lib.rs`. |
| `ndb-cli` | Command-line client over the HTTP wire protocol — `health/read/commit/iter/flush/compact/lookup/vector-search/property-lookup/property-range`. `NDB_URL`/`NDB_TOKEN`. Bin `main.rs`. |
| `ndb-slicer` | CPU + GPU projection/aggregation (§7, §17.1) over a `Record` stream — `select_columns`, `filter`, `group_by`, aggregates (Count/Sum/Avg/Min/Max), `sort`, `limit`. Files: `lib.rs`, `batch.rs`, `gpu.rs`, `sum_reduce.wgsl`. |
| `ndb-renderer` | Turn a slicer `Table` into text/TSV/CSV (§17.1, 2D) — `render_text`, `render_tsv`, `render_csv`. Files: `lib.rs`, `viz.rs`. |
| `ndb-mcp-server` | Model Context Protocol server — JSON-RPC 2.0 over stdio, embedded engine (no HTTP hop). Tools (`ndb.health/read/commit_entity/iter/...`), resources (`ndb://schema/dictionaries/stats`), prompts. Bin `main.rs` + `lib.rs`. |
| `ndb-arrow` | Apache Arrow IPC interop — bridges `snapshot_iter` to `RecordBatch`/IPC bytes for Polars/pandas/DuckDB/cuDF. `records_to_batch(es)`, `records_to_ipc_stream_chunked`, `vector_column_batch` (cuVS), `hyperedge_edge_index` (cuGraph/PyG). Single `lib.rs`. |
| `ndb-index-vector-hnsw` | Opt-in HNSW ANN index (§14.2), same surface as engine's brute-force `VectorIndex`; backed by pure-Rust `instant-distance`. L2-sq / cosine. Single `lib.rs`. |
| `ndb-client-rust` | Rust HTTP wire client (§17.1) — typed `Client` mirroring server routes, `std::net::TcpStream` (no async). Reuses engine `wire::*` types. `ClientError` (Io/…). Single `lib.rs`. |
| `ndb-router` | Stateless sharding coordinator — same `/v1` wire protocol, fans out across N single-writer shards. `hash(entity_id) % N`; hash-first reads w/ scatter-on-miss, split commits (entity→owner, hyperedge→anchor, dict→broadcast), scatter+merge scans + vector kNN. Bin `main.rs` + `lib.rs`. Depends on `ndb-client-rust`. |
| `ndb-query` | Query-language lexer + parser: text → name-based AST (`NameQuery`); engine resolver turns it into id-based `QueryRequest`. Spans for errors. Files: `lex.rs`, `parse.rs`, `ast.rs`, `resolve.rs`, `run.rs`, `error.rs`. |
| `ndb-studio` | Standalone app: open any nDB as tables, creative projections, versioned edits w/ time-travel; engine in-process + embedded single-file web UI. Layers: `store` (only code touching engine), `jsonval`, `http`, `identity`. Bin `main.rs`. |
| `nstack` | Typed n-dimensional ERP kernel on nDB (Phase 1 slice) — compile-time currency/lifecycle safety (`money::Money`, `sales::SalesOrder`), commit-time invariants, `testkit::TestDb` in-process harness. Single `lib.rs`. |
| `bench-race-sqlite-rust` | SQLite race-bench server via `rusqlite` (no GIL) — fair-architecture comparison vs the Python sqlite3 lane. Same HTTP race API (`/health`, `/workloads`, `/stats`, `/run/<name>`, `/stress`). Bin `main.rs`. |
| `tools/langgraph` | Rust-native GraphRAG ingestor for demos — builds an OpenAlex (CC0) citation graph directly through the embedded engine (no seed.json hop). 5-D demo: x/y/z layout, year→time scrubber, embedding→vector_search. Files: `main.rs`, `server.rs`. |

### Cross-crate dependency shape
- `ndb-server`, `ndb-cli`, `ndb-mcp-server`, `ndb-arrow`, `ndb-slicer`, `ndb-renderer`, `ndb-index-vector-hnsw`, `ndb-client-rust`, `ndb-query`, `ndb-studio`, `nstack`, `tools/langgraph` all depend on **`ndb-engine`** (the core everything builds on).
- `ndb-router` depends on **`ndb-client-rust`** (speaks the wire protocol to shards).
- `ndb-server` also pulls `ndb-query`, `ndb-arrow`, `ndb-client-rust`.
- `ndb-mcp-server` also pulls `ndb-server` + `ndb-arrow`.
- `ndb-renderer` pulls `ndb-slicer`. `bench-race-sqlite-rust` is independent (rusqlite only).

## Clients & deploy
- **`clients/ts`** — `@ndb/client` (v0.1.0): thin typed TS SDK for wire protocol v1, zero deps, runs on Node/browser/Deno/edge. `src/index.ts` → `dist/index.js`.
- **`clients/mcp-npm`** — `@ndb/mcp` (v0.1.0): one-command launcher (`ndb-mcp` bin) pointing Claude/Cursor/Codex at an nDB database; wraps `ndb-mcp-server`. `bin/ndb-mcp.js`.
- **`clients/python`** — Python wire client (`ndb_client/`, `pyproject.toml`, tests).
- **`Dockerfile`** + **`docker-compose.yml`** (single server) + **`docker-compose.sharded.yml`** (router + N shards).
- **`deploy/helm/ndb`** — Helm chart (`Chart.yaml`, `values.yaml`); `deploy/README.md`.

## Key docs
- `docs/PROTOCOL.md` — wire protocol v1 (JSON over HTTP/1.1); the stable SDK contract.
- `docs/COMPATIBILITY.md` — compatibility & versioning policy ("your data always opens").
- `docs/PRODUCTION.md` — what the engine guarantees, how to operate, what is NOT yet covered.
- `docs/QUICKSTART.md` — two ≤5-min paths (app developer + AI coding agent).
- `docs/explorer/PERFORMANCE.md` — measured explorer-on-production-engine performance; `docs/explorer/index.html` is the 3D/5D explorer UI.
- `docs/superpowers/specs/` — authoritative design specs:
  - `2026-05-27-nDB-hypergraph-design.md` — core architecture (the §-references throughout the code).
  - `2026-05-27-query-language.md` — query language spec (`ndb-query`).
  - `2026-05-27-v2-working-spec.md`, `2026-05-27-v2-1-working-spec.md` — v2 / v2.1 working specs.
  - `2026-05-31-ndb-studio-design.md` — Studio design.
  - `2026-06-13-ndb-adoptable-core-design.md` — adoptable/low-RAM core.
  - `2026-06-14-ndb-deploy-operate-design.md` — deploy & operate.
  - `2026-06-14-ndb-scale-sharding-design.md` — sharding (`ndb-router`).
- Other: `docs/nDB-whitepaper.md`, `docs/architecture/`, `docs/gpu-dgx-spark.md` (unified-memory GPU path), `docs/benchmarks/`, domain demos (`alphafold_nDB`, `chemistry`, `exoplanet`, `biodiv`, `seismic`, `langgraph`, `knowledge-site`), `docs/ndb-vs-mariadb-storage{,-en}.md`.
