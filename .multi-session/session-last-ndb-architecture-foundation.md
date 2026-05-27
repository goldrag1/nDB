## Session 2026-05-27 — nDB v1 Storage Core + Indexes + Compaction + Validation (single mega-session)

### Đã làm

Implemented the bulk of the v1 storage layer end-to-end. Started from a documentation-only repo; ended with a working `ndb-engine` Rust crate that supports create → write → snapshot read → flush → compact → restart → re-read, with three of the six mandatory v1 indexes, runtime validation constraints, and an end-to-end example binary. **135 tests passing, clippy clean with `-D warnings`**.

**Branch:** `feature/storage-core` off `develop` (not yet merged, not yet pushed — no remote configured).

### Commits on `feature/storage-core` (13 commits, ~8,937 LOC added)

| Commit | What |
|---|---|
| `2489280` | docs: rename `*DefRecord` → `*NameRecord`/`*KeyRecord`, pin sentinels (`TX_ACTIVE`, `TYPE_UNTYPED`), `record_size` self-inclusive, `arity ≥ 1`, `format_version` byte |
| `d1c0381` | feat(engine): record layouts, `Value` tagged union, UUID v7 identifiers (§11.2/§11.3). Newtype IDs (`EntityId`, `HyperedgeId`, `TypeId`, `RoleId`, `PropertyId`, `TxId`). |
| `b49e7a2` | feat(engine): WAL (`.ndblog`). Buffered file I/O, torn-record-aware recovery. |
| `1db6add` | feat(engine): SSTable (`.ndb`). Sort key = `(record_kind, primary_id)`. Atomic publish via write-temp + fsync + rename + fsync_dir. |
| `1900235` | feat(engine): database directory — `MANIFEST` (versioned snapshot), `CURRENT` (text pointer), `LOCK` (stdlib `File::try_lock`). |
| `ad88445` | feat(engine): memtable (`BTreeMap<SSTableKey, Vec<Record>>`) + MVCC visibility resolver. `Memtable::lookup_by_uuid` aggregates Entity + HyperEdge + Tombstone buckets. |
| `910fd25` | feat(engine): `Engine` — keystone. `begin_write`/`commit`/`rollback`, `snapshot_read`, `flush`. WAL replay + MANIFEST staleness reconciliation. |
| `53365c3` | session: mid-session summary (now superseded by this file). |
| `c1be6e2` | feat(engine): three indexes — LookupKeyIndex, AdjacencyIndex, HyperEdgeTypeIndex. In-memory, rebuilt on Engine::open, updated synchronously in commit. |
| `2b7de81` | feat(engine): full compaction. Two-pass: build `killed: HashMap<Uuid, TxId>` from cross-bucket tombstones, then emit survivors. Drops superseded versions + tombstoned records. |
| `f1d008b` | feat(engine): validation engine. `require_property` + `expect_value_tag`. Validation runs FIRST in commit; atomic abort. |
| `dd3f2de` | docs: `examples/basic.rs` — end-to-end demo runnable via `cargo run -p ndb-engine --example basic`. |
| (this file) | session-close. |

### v1 Decisions Now Locked In Code

Each is documented in the relevant module preamble:

| Concern | Decision | Module |
|---|---|---|
| Sort key for primary store | `(record_kind, primary_id)` lexicographic | `sstable.rs` |
| WAL strategy | Separate `.ndblog` file; buffered I/O; not mmap | `wal.rs` |
| MANIFEST encoding | Versioned full-snapshot (not edit-log) | `db.rs` |
| LOCK | stdlib `File::try_lock` (1.89+) | `db.rs` |
| MVCC supersession | Datomic-style derive-at-read; `tx_id_supersede = TX_ACTIVE` on all live records | `mvcc.rs` |
| Memtable | `BTreeMap<SSTableKey, Vec<Record>>` multi-version | `memtable.rs` |
| Single-writer | `&mut self` for writes, `&self` for reads; no embedded locks | `engine.rs` §14.3 |
| Indexes | In-memory, rebuilt on open, updated in commit; no on-disk sidecar yet | `index/mod.rs` |
| Lookup-key indexable Values | All atomic Value tags except Null and Extension | `index/lookup_key.rs` |
| Adjacency index granularity | `entity → BTreeSet<HyperedgeId>`, no role info (v2 may add) | `index/adjacency.rs` |
| Compaction | Full merge to single L1 SSTable; no snapshot tracking; drops tombstones immediately | `engine.rs::compact` |
| Validation | Required-property + value-tag only; runtime registration | `validation.rs` |

### Trạng thái hiện tại

```
nDB-ndimemsion-database/
├── Cargo.toml + Cargo.lock + rust-toolchain.toml + .gitignore
├── crates/ndb-engine/
│   ├── Cargo.toml
│   ├── examples/
│   │   └── basic.rs               # cargo run --example basic
│   └── src/
│       ├── lib.rs                 # public re-exports
│       ├── codec.rs               # LE read/write primitives + Cursor
│       ├── error.rs               # EncodeError + DecodeError
│       ├── id.rs                  # newtype IDs + sentinels
│       ├── value.rs               # Value tagged union (11 tags)
│       ├── record.rs              # 6 record kinds + Record enum
│       ├── wal.rs                 # WriteAheadLog + WalReader + recovery
│       ├── sstable.rs             # SSTableWriter + SSTableReader + SSTableKey
│       ├── db.rs                  # Database, Manifest, LOCK/CURRENT
│       ├── memtable.rs            # in-memory BTreeMap multi-version store
│       ├── mvcc.rs                # Resolved, resolve_iter, visible_at
│       ├── engine.rs              # Engine + WriteTxn + recovery + compact
│       ├── index/
│       │   ├── mod.rs             # Index trait + re-exports
│       │   ├── lookup_key.rs      # (property_id, value) → entity_id
│       │   ├── adjacency.rs       # entity → {hyperedges}
│       │   └── type_cluster.rs    # type_id → {hyperedges}
│       └── validation.rs          # required-property + value-tag enforcement
├── docs/                          # design spec + white paper
└── .multi-session/                # this file
```

Test count: **135 passing, 0 failing**. `cargo clippy --all-targets -- -D warnings` clean. `cargo run --example basic` runs end-to-end and demonstrates every shipped feature.

### Bugs caught + fixed inline this session

1. **WAL "torn trailing record" pos discipline.** Initial implementation set `pos = file_len` on detecting partial trailing record → `trailing_garbage` always 0, recovery silently corrupted. Fix: leave pos UNCHANGED on partial detection so `durable_end` lands exactly at the boundary.
2. **Memtable per-key vs cross-bucket lookup.** Entity records and tombstones for the same UUID land in different `SSTableKey` buckets (kind byte differs). Per-key lookup misses the tombstone. Fix: `Memtable::lookup_by_uuid` aggregates across the three UUID-bearing kinds.
3. **MANIFEST staleness on restart.** Post-flush commits live in the new WAL but don't bump `manifest.last_tx_id` until the next flush. After close + reopen, `last_tx_id` was the flush-time value, making replayed records invisible. Fix: `replay_wal_into` returns `(safe_end, max_tx_seen)`, Engine::open reconciles + persists.
4. **Compaction cross-bucket tombstone.** Same bucket-mismatch issue as #2. Pass 1 builds `killed: HashMap<Uuid, TxId>` from every tombstone; Pass 2 consults it to drop entities + their tombstones together.

### Next Session Task

**Wire protocol + companion crates.** The engine is genuinely usable as a Rust library. To complete v1 (per §17.1) the remaining work is:

1. **Wire protocol** (HTTP + JSON + JSONL streaming, §4 architecture overview). Minimum viable surface:
   - `POST /commit` — request body has Records (JSON shape TBD), response is `{"tx_id": N}`.
   - `GET /read/:uuid` — response is JSON record or 404.
   - `POST /query` — Datalog-influenced query (defer until query language exists).
   - `GET /health` — liveness probe.

   Recommended stack: pure `std::net::TcpListener` + handwritten HTTP/1.1 parser for v1 (no async, no axum, no hyper). Single-threaded matches our single-writer model. ~300 LOC, no dep cost. Add tokio + axum in v2 when concurrency matters.

2. **nDB-slicer** (§7, §17.1) — CPU projection + aggregation. Reads `Engine::snapshot_iter` output, applies group-by + aggregate (sum/avg/count) + sort + limit. Output is a tabular `Vec<Vec<Value>>` ready for renderers.

3. **nDB-renderer** (2D outputs: table, scatter, pivot, bar/line/area). Stub it for v1 — even text-table output gets us a "you can see your data" feel.

4. **nDB-cli** (REPL + admin). Consumes wire protocol. Stub: print "v1: query language pending" and just demonstrate commits/reads/iter via JSON CLI args.

5. **nDB-mcp-server** — Model Context Protocol bridge. Likely a separate stdio binary using the Rust `mcp-sdk` (when stable) or hand-rolled JSON-RPC.

6. **nDB-index-vector-cpu** (HNSW) — vector similarity index. Pure-CPU; can ship as a separate crate consuming the Engine's snapshot iter for embedding-bearing records.

7. **Arrow IPC interop** — Zero-copy bridge to Polars/pandas/DuckDB. Needs `arrow-rs` dep; emits `Record` columns as Arrow batches.

8. **Security baseline** (§13) — Bearer tokens + TLS + ReBAC capability hyperedges + filesystem encryption. Cross-cutting; defer until wire protocol exists (token check belongs on the request path).

9. **Block index sidecar** (`<seq>.idx`) — makes SSTable `find` O(log N) instead of O(N). Big perf win once datasets grow.

10. **Property B-tree index** — the 6th mandatory index. Needs Value-ordering semantics that haven't been pinned yet (separate spec decision).

11. **Query language** (§12) — pattern matching + Datalog suffix syntax. Big design task; needs its own focused spec.

**Concrete starting sequence for the next session:**

1. Read this file + `docs/superpowers/specs/2026-05-27-nDB-hypergraph-design.md` (especially §4, §12, §17.1).
2. Decide on a "wire protocol vs companion crate first" priority. My recommendation: **wire protocol first** because nothing else is testable from outside Rust without it.
3. For the wire protocol: add a `crates/ndb-server/` crate with the HTTP server, request/response types serializable via `serde_json`. Engine instance lives inside; serve a single port.
4. Add an integration test that does `curl POST /commit` then `curl GET /read/:uuid` against a real running server.
5. Add `serde` + `serde_json` to workspace deps. Versions: `serde = "1"`, `serde_json = "1"`.

**Architectural constraints to remember (locked, do NOT re-litigate):**

- Engine is single-writer + `&mut self`. The wire-protocol server must serialize writers (mutex or actor-style task).
- JSON-shape decisions: prefer record-as-JSON-object with explicit `_kind` discriminator: `{"_kind": "Entity", "entity_id": "...", "type": 1, ...}`. Match the rendered shape to the existing `Record` enum.
- JSONL streaming responses for `/query` and any future iteration endpoint.
- Validation happens server-side (already integrated in `WriteTxn::commit`); the wire protocol just passes records through.

### Remaining Acceptance Criteria for v1

These are still open and explicitly punted to future sessions:

- [ ] Block index sidecar + O(log N) SSTable find (§11.5, `<seq>.idx`)
- [ ] Schema-driven property B-tree (6th mandatory index)
- [ ] Vector index algorithm (HNSW vs IVF vs ScaNN)
- [ ] Block size + alignment for SSTables
- [ ] Compression algorithm + block size (Zstd vs LZ4)
- [ ] Query language formal grammar
- [ ] Wire protocol JSON schema
- [ ] Snapshot-aware compaction (track oldest live snapshot)
- [ ] Validation from metadata hyperedges (vs runtime registration in v1)
- [ ] Crash-injection tests (currently we test the recovery paths via simulated corruption only)

### Learnings worth keeping

Reinforce patterns already in `~/.claude/rules/programming.md` and `multi-session.md`:

- **Bake decisions into the code that implements them.** Every module preamble in this session contains a "v1 decisions baked in here" block. Future readers find the rationale next to the implementation.
- **Test failure modes first.** Every storage component has a corruption / truncation / wrong-magic test alongside the happy-path round-trip. The MANIFEST staleness bug, the WAL pos bug, and the compaction cross-bucket tombstone bug were ALL caught by tests written before the implementation was considered done.
- **Sentinel discipline must be enforced symmetrically at encode AND decode.** Encoders reject zero `role_id`/`prop_id`/`type_id` where forbidden; decoders also reject these sentinels. Without symmetry, a tampered file could carry illegal values past the parser.
- **CRC-checked envelopes + self-inclusive `record_size` + magic bytes + `format_version` byte** is the standard envelope pattern for every file format in nDB (records, SSTable footer, MANIFEST). Worth lifting into a shared module when the next file format lands (block-index sidecar will be the catalyst).
- **In-memory indexes rebuilt on open** is a real v1 design pattern, not a punt. It keeps the write path clean (no extra sidecar to keep durable), trades startup time for write throughput, and lets every index test stay self-contained without I/O.
- **Cross-bucket awareness in compaction.** Entity records and tombstones for the same UUID sort to different keys. Any merging/joining process must consult both. Easy to miss; easy to test for.
- **Atomic validation before WAL durability.** Reject early, before anything touches disk. Validation errors should never leave half-committed state.

No new cross-project rules promoted — these are all reinforcements of existing rules.
