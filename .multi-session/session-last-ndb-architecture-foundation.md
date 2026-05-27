## Session 2026-05-27 — nDB v1 storage core + companion crates + wire + AI bridge

### Đã làm

Implemented the v1 storage core end-to-end AND the full v1 companion-crate
stack. Starting from a documentation-only repo, this session shipped a
working hypergraph database with HTTP wire protocol, CLI client, MCP server
for AI agents, CPU slicer, text/CSV renderer, and all six mandatory indexes.

**193 tests passing across the workspace, clippy clean with `-D warnings`,
24 commits on `feature/storage-core`.**

`README.md` written at the repo root explaining the workspace, the wire
protocol, the on-disk layout, and what's shipped vs. deferred to v2.

### Workspace shipped

| Crate              | Lines     | Tests | Role                                                                                  |
|--------------------|-----------|-------|---------------------------------------------------------------------------------------|
| `ndb-engine`       | ~6500     | 163   | Storage core: records, WAL, SSTable, MANIFEST, memtable, MVCC, compaction, validation, 6 indexes |
| `ndb-server`       | ~600      | 8     | Hand-rolled HTTP/1.1 server, bearer-token auth, JSONL streaming                       |
| `ndb-cli`          | ~225      | 0     | `ndb` binary, HTTP client                                                             |
| `ndb-mcp-server`   | ~600      | 8     | Stdio JSON-RPC MCP bridge for AI agents                                               |
| `ndb-slicer`       | ~600      | 8     | CPU projection / filter / group-by / aggregate / sort / limit                         |
| `ndb-renderer`     | ~270      | 6     | Bordered ASCII text + TSV + CSV outputs                                               |

### Commits on `feature/storage-core` (24)

Storage core (8):
- `2489280` prep: rename `*DefRecord` → `*Name`/`*Key`, sentinels named, `record_size` self-inclusive
- `d1c0381` record layouts + Value + UUID v7 identifiers (§11.2/§11.3)
- `b49e7a2` WAL (`.ndblog`) with torn-record recovery (§9.1, §11.5)
- `1db6add` SSTable (`.ndb`) with atomic publish (§11.5)
- `1900235` database directory — MANIFEST + CURRENT + LOCK (§11.5)
- `ad88445` memtable + MVCC visibility (§10)
- `910fd25` Engine — transactions, snapshot reads, flush + restart (§10, §14.3)
- `53365c3` session-last update (mid-arc)

Indexes + compaction + validation (4):
- `c1be6e2` three indexes — lookup-key + adjacency + hyperedge-type-cluster (§14.2)
- `2b7de81` full compaction — drop superseded versions + tombstoned records
- `f1d008b` validation engine — required properties + value-tag constraints (§6.3)
- `dd3f2de` `examples/basic.rs` — end-to-end demo

Wire + clients + AI (5):
- `d43886e` JSON wire format for Value + Record (§4)
- `c1d236b` ndb-server crate — HTTP/1.1 wire-protocol bridge (§4)
- `c377534` ndb CLI + /flush + /compact endpoints
- `c4a644a` property B-tree — 6th mandatory v1 index (§14.2)
- `87f613a` bearer-token auth on the wire (§13.1)

Analytics + AI (5):
- `65502f1` ndb-slicer crate — CPU projection + aggregation (§7, §17.1)
- `c1d521d` ndb-renderer crate — text/TSV/CSV outputs
- `00d19c8` vector index — brute-force CPU k-NN (§14.2)
- `b547e2e` ndb-mcp-server crate — AI-agent bridge via stdio JSON-RPC (§17.1)
- `93983a9` session-close at mid-arc (now superseded by this file)

Final consolidation (2):
- (this file)
- README.md at the repo root

### v1 deliverable checklist (§17.1)

| Deliverable                            | Status |
|----------------------------------------|--------|
| Engine + 6 mandatory indexes           | ✅ shipped |
| nDB-slicer (CPU projection/aggregation)| ✅ shipped |
| nDB-renderer (2D text/TSV/CSV)         | ✅ shipped |
| Validation engine                       | ✅ shipped (runtime registration; metadata-hyperedge-driven deferred) |
| Vector index                           | ✅ shipped (brute-force CPU; HNSW deferred) |
| nDB-client-rust (CLI)                  | ✅ shipped |
| nDB-cli                                | ✅ shipped (`ndb` binary) |
| nDB-mcp-server                         | ✅ shipped (stdio JSON-RPC) |
| Wire protocol (HTTP + JSON + JSONL)    | ✅ shipped |
| Bearer-token auth                      | ✅ shipped |
| nDB-client-python                      | ❌ separate language project |
| Arrow IPC interop                      | ❌ deferred (arrow-rs dep cost; CSV fallback exists via renderer) |
| Full security baseline (TLS, ReBAC, audit) | ⚠️ partial — bearer tokens only |
| Block index sidecar (`<seq>.idx`)       | ❌ deferred |
| Snapshot-aware compaction              | ❌ deferred |
| Query language                         | ❌ §12.9 grammar still open; needs own session |

### Locked v1 decisions (in module preambles)

| Concern | Decision | Module |
|---|---|---|
| Sort key for primary store | `(record_kind, primary_id)` lexicographic | `sstable.rs` |
| WAL strategy | Separate `.ndblog`, buffered I/O, not mmap | `wal.rs` |
| MANIFEST encoding | Versioned full-snapshot (not edit-log) | `db.rs` |
| LOCK | stdlib `File::try_lock` (1.89+) | `db.rs` |
| MVCC supersession | Append-only, derive-at-read | `mvcc.rs` |
| Memtable | `BTreeMap<SSTableKey, Vec<Record>>` multi-version | `memtable.rs` |
| Concurrency | Single-writer (`&mut self` for writes), no embedded locks | `engine.rs` |
| Index lifecycle | In-memory, rebuilt on open, updated on commit | `index/mod.rs` |
| Lookup-key indexable values | All atomic Value tags except Null and Extension | `index/lookup_key.rs` |
| Adjacency granularity | `entity → BTreeSet<HyperedgeId>` (no role info; v2 may add) | `index/adjacency.rs` |
| Property B-tree value encoding | Sign-bit-flipped BE for ints/decimals/timestamps; IEEE-trick for floats; raw bytes for strings | `index/property_btree.rs` |
| Vector index distance | L2Squared and Cosine; brute force; HNSW deferred | `index/vector.rs` |
| Compaction | Full merge to single L1 SSTable; no snapshot tracking | `engine.rs::compact` |
| Validation | Required-property + value-tag only; runtime registration | `validation.rs` |
| Wire format | Tagged-union for Value; "active" sentinel for tx_id_supersede; base64 for bytes; i128 mantissa as string | `wire.rs` |
| Server transport | Hand-rolled HTTP/1.1 over std::net, single-threaded, no tokio | `ndb-server/src/lib.rs` |
| Auth | Bearer token, constant-time compare; /health always open | `ndb-server/src/lib.rs` |
| MCP transport | JSON-RPC 2.0 over stdio, newline-framed | `ndb-mcp-server/src/lib.rs` |
| MCP surface | tools/* only; resources + prompts in v2 | `ndb-mcp-server/src/lib.rs` |

### Bugs caught + fixed inline this session

1. **WAL torn-trailing-record pos discipline** — setting `pos = file_len` on partial detection made `trailing_garbage` always 0. Fix: leave pos at the partial-record boundary.
2. **Memtable `lookup_by_uuid` aggregates 3 buckets** — entity record and its tombstone sort to different `SSTableKey`s; lookup must consult all three UUID-bearing kinds.
3. **MANIFEST staleness on restart** — post-flush WAL commits aren't reflected in MANIFEST.last_tx_id; reconcile during replay.
4. **Compaction cross-bucket tombstone** — entity and tombstone at different keys; two-pass approach: build `killed: HashMap<Uuid, TxId>` first, then emit survivors filtered by it.
5. **`Duration::from_secs(60)` triggers clippy `duration_suboptimal_units`** in 1.95 — use `from_mins(1)`.

### Bench-of-bench: what works end-to-end

The README's "Quick start" was verified by hand against:

- `cargo run -p ndb-engine --example basic` — full lifecycle in process: validation reject, commit, snapshot read, lookup-key, adjacency, type cluster, tombstone, flush, compact, restart, re-verify.
- `ndb-server` + `ndb` CLI: health → commit → read → iter → flush over loopback HTTP, with and without bearer token.
- `ndb-mcp-server` over piped stdin: initialize → tools/list → ndb.health → ndb.commit_entity → ndb.iter.

### Next session priorities (when work resumes)

1. **Arrow IPC interop** — `crates/ndb-arrow` reading Engine output → `RecordBatch`. Big dep (arrow-rs) but unlocks Polars / pandas / DuckDB zero-copy.
2. **Block index sidecar** (`<seq>.idx`) — make `SSTableReader::find` O(log N). Substantial change to writer; defer until perf is a real complaint.
3. **Query language** (§12) — Datalog-influenced pattern matching. Needs its own focused spec before code. Start with a grammar.
4. **TLS + ReBAC capabilities** — finish the security baseline. TLS termination via rustls; ReBAC via capability hyperedges (already-shipped Engine concept).
5. **Python client** — separate language stack; out of in-session scope here.
6. **Snapshot-aware compaction** — track oldest live snapshot, only drop versions older than it. Removes the v1 "compaction forfeits in-flight reads" limitation.

### Learnings worth keeping

These are reinforcements of existing rules in `~/.claude/rules/`, not new ones — the patterns played out exactly the way the rule files predicted:

- **Bake decisions into the code that implements them.** Every module preamble in every crate carries a "v1 decisions baked in here" block. Future readers find rationale next to implementation.
- **Test the failure modes first.** Every record layout, WAL recovery, SSTable footer, MANIFEST encoding, index out-of-order has a torn / corrupted / wrong-magic test alongside the happy-path round-trip.
- **Sentinel discipline at encode AND decode.** Encoders reject illegal zeros; decoders also reject them; sentinels live in named constants (`TX_ACTIVE`, `TYPE_UNTYPED`) not parenthetical comments.
- **Cross-bucket awareness in compaction and lookup.** Entity records and tombstones for the same UUID sort to different keys; any joining process must consult both.
- **CRC-checked envelopes + self-inclusive size + magic + format_version** is the file-format pattern across records, SSTable footer, MANIFEST, and will be in any future v1 sidecar file.
- **In-memory indexes rebuilt on open** keeps the write path clean (no extra sidecar to keep durable), trades startup time for write throughput, and lets every index test stay self-contained without I/O. v2 can persist them.
- **Atomic validation before WAL durability.** Reject early, before anything touches disk.
- **Hand-rolled HTTP/1.1 in pure `std::net`** is a real path for a v1 single-writer database. No tokio, no axum, no hyper dep — ~250 LOC and it works.
- **MCP-as-stdio-JSON-RPC** is the lowest-friction AI-agent integration; ~600 LOC including 8 tools.

No new cross-project rules promoted.
