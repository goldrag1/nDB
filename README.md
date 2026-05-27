# nDB

An n-dimensional hypergraph database engine in Rust.

This repository contains the v1 storage core and companion crates. nDB is
**hyperedge-native**: the atomic unit is an N-ary fact connecting any number of
entities in named role slots — not a row in a table, and not a binary edge with
reified properties. See the [white paper](docs/nDB-whitepaper.md) for the
architectural narrative and the [full design spec](docs/superpowers/specs/2026-05-27-nDB-hypergraph-design.md)
for the byte-level details.

## Workspace

| Crate              | Purpose                                                                              |
|--------------------|--------------------------------------------------------------------------------------|
| `ndb-engine`       | Storage core: records, WAL, SSTable, MANIFEST, memtable, MVCC, compaction, indexes, validation. Library only. |
| `ndb-server`       | Hand-rolled HTTP/1.1 server exposing the engine over JSON. Binary `ndb-server`.      |
| `ndb-cli`          | Command-line client. Talks to `ndb-server` over HTTP. Binary `ndb`.                  |
| `ndb-mcp-server`   | Model Context Protocol bridge — exposes the engine to AI agents via stdio JSON-RPC. Binary `ndb-mcp-server`. |
| `ndb-slicer`       | CPU projection + aggregation over `Engine::snapshot_iter` output.                    |
| `ndb-renderer`     | Text/TSV/CSV table output for `ndb-slicer` results.                                  |

## Status (v1)

Storage core is end-to-end working. Wire protocol, CLI, MCP server, and the data
pipeline all pass tests against real databases on disk. **193 tests, clippy
clean with `-D warnings`** as of this writing.

What's shipped:

- Append-only LSM storage (records, WAL `.ndblog`, SSTable `.ndb`, MANIFEST, CURRENT, LOCK)
- MVCC with snapshot reads + supersession derived at read time
- Single-writer transaction commit with validation pre-check
- All 6 mandatory v1 indexes (entity-by-id, hyperedge-by-id, lookup-key, adjacency, hyperedge-type-cluster, property B-tree)
- Brute-force vector index (k-NN, L2 / cosine)
- Full compaction with cross-bucket tombstone handling
- JSON wire protocol over HTTP/1.1 with bearer-token auth
- CLI client over HTTP
- MCP server over stdio JSON-RPC
- CPU slicer (project, filter, group-by, sum/avg/count/min/max, sort, limit)
- Text/TSV/CSV renderer

What's deferred to v2:

- Block index sidecar (`<seq>.idx`) for O(log N) SSTable lookups
- Snapshot-aware compaction (track oldest live snapshot)
- Real ANN algorithm (HNSW vs IVF vs ScaNN)
- TLS termination in the server (today: terminate at the reverse proxy)
- ReBAC capability hyperedges, audit logging
- Validation driven by metadata hyperedges (today: runtime `Engine::require_property` etc.)
- Query language (§12) — Datalog-influenced pattern matching
- Python client
- Arrow IPC interop
- Distributed mode (v3+)

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

| Method | Path           | Body                                  | Response                                      |
|--------|----------------|---------------------------------------|-----------------------------------------------|
| GET    | `/health`      | —                                     | `{"status":"ok"}`                             |
| POST   | `/commit`      | `CommitRequest { records: [...] }`    | `CommitResponse { tx_id }`                    |
| GET    | `/read/:uuid`  | —                                     | `ReadResponse { outcome: missing|deleted|live, ... }` |
| GET    | `/iter`        | —                                     | JSONL stream of records                       |
| POST   | `/flush`       | —                                     | memtable + SSTable counts                     |
| POST   | `/compact`     | —                                     | `CompactionStats`                             |

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
