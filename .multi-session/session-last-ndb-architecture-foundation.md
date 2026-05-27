## Session 2026-05-27 — nDB Storage Core v1 (end-to-end working)

### Đã làm

Implemented the v1 storage core end-to-end. Started from a documentation-only repo; ended with a working `ndb-engine` Rust crate (Cargo workspace, edition 2024) that supports create → write → snapshot read → flush → restart → read again. **98 tests passing, clippy clean with `-D warnings`**.

**Branch:** `feature/storage-core` off `develop` (not yet merged, not yet pushed — no remote configured).

### Commits on `feature/storage-core` (7 commits, ~6,749 LOC added)

| Commit | What |
|---|---|
| `2489280` | docs: rename `*DefRecord` → `*NameRecord`/`*KeyRecord`, pin sentinels (`TX_ACTIVE`, `TYPE_UNTYPED`), `record_size` self-inclusive, `arity ≥ 1`, `format_version` byte (was overloaded "version") |
| `d1c0381` | feat(engine): record layouts, `Value` tagged union, UUID v7 identifiers (§11.2/§11.3). Newtype IDs (`EntityId`, `HyperedgeId`, `TypeId`, `RoleId`, `PropertyId`, `TxId`). Sentinel discipline at encode AND decode. |
| `b49e7a2` | feat(engine): WAL (`.ndblog`). Separate file (not memtable-as-WAL). `create`/`open_append`/`append`/`append_batch`/`sync`. Reader handles torn trailing records by keeping pos at the boundary so `durable_end` lands exactly on safe-truncate. |
| `1db6add` | feat(engine): SSTable (`.ndb`). Sort key = `(record_kind, primary_id)` — closes one §11.4 sub-question. Atomic publish via `write-temp + fsync + rename + fsync_dir`. `SSTableWriter`/`SSTableReader`/`SSTableKey`/`read_footer`. |
| `1900235` | feat(engine): database directory — `MANIFEST` (versioned snapshot, not edit-log), `CURRENT` (text pointer, atomic rewrite), `LOCK` (stdlib `File::try_lock`, stable 1.89+). `Database::create`/`open`/`write_manifest`/`allocate_file_seq`/`allocate_tx_id`. |
| `ad88445` | feat(engine): memtable (`BTreeMap<SSTableKey, Vec<Record>>`) + MVCC visibility resolver. `Memtable::lookup_by_uuid` aggregates Entity + HyperEdge + Tombstone for the same UUID (they sort to different buckets but represent one logical record). `mvcc::resolve_iter` is the canonical visibility function. |
| `910fd25` | feat(engine): `Engine` — the keystone. Ties Database + WAL + Memtable + SSTables into one handle. `begin_write`/`commit`/`rollback`, `snapshot_read`, `snapshot_iter`, `flush`. Recovery flow: acquire LOCK → open SSTables → replay WAL → reconcile `manifest.last_tx_id` with replayed max-tx (caught a real bug — records committed after a flush were invisible on reopen). |

### Quyết định kỹ thuật quan trọng (locked in code + commit messages)

| Concern | Decision | Source |
|---|---|---|
| Sort key for primary store | `(record_kind, primary_id)` — `entity_id`/`hyperedge_id`/`target_id` raw bytes for UUID records; `u32` BE for dictionary | §11.4 sub-Q1, locked in `1db6add` |
| WAL vs memtable | Separate `.ndblog` file (not memtable-as-WAL) | §11.4 sub-Q3, locked in `b49e7a2` |
| Mmap vs buffer pool | `BufReader`/`BufWriter` for v1; defer mmap until benchmarks justify | §11.4 sub-Q4, locked in `b49e7a2`/`1db6add` |
| MANIFEST encoding | Versioned full-snapshot, not edit-log (RocksDB-style log defers to v2 if needed) | `1900235` |
| MVCC supersession | Append-only purist — `tx_id_supersede = TX_ACTIVE` always on new records; supersession derived from `tx_id_assert` ordering at read time (Datomic model, not Postgres in-place update) | `ad88445` |
| Block size + alignment | Still deferred (§11.4 sub-Q1). v1 SSTable has no block index; lookups linear-scan. |
| Compression | Still deferred. Per-record CRC + per-block compression both punt to follow-on work. |
| WAL durability | `BufWriter::flush()` + `File::sync_data()` per `WriteAheadLog::sync()`. Batches committed in one call become atomically durable. |
| Crash safety | `write-temp + fsync + rename + fsync_dir` for SSTable and MANIFEST. WAL recovery truncates at the first torn-record boundary. |

### Trạng thái hiện tại

```
nDB-ndimemsion-database/
├── Cargo.toml + rust-toolchain.toml + .gitignore + Cargo.lock
├── crates/
│   └── ndb-engine/
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs       # public re-exports
│           ├── codec.rs     # LE read/write primitives + Cursor
│           ├── error.rs     # EncodeError + DecodeError
│           ├── id.rs        # newtype IDs + sentinels
│           ├── value.rs     # Value tagged union (11 tags)
│           ├── record.rs    # 6 record kinds + Record enum + envelope helpers
│           ├── wal.rs       # WriteAheadLog + WalReader + recovery
│           ├── sstable.rs   # SSTableWriter + SSTableReader + SSTableKey
│           ├── db.rs        # Database, Manifest, ManifestEntry, LOCK/CURRENT
│           ├── memtable.rs  # in-memory BTreeMap multi-version store
│           ├── mvcc.rs      # Resolved, resolve_iter, visible_at, effective_tx
│           └── engine.rs    # Engine + WriteTxn + recovery flow
├── docs/                    # design spec + white paper (updated this session)
└── .multi-session/          # this file
```

Test count: **98 passing, 0 failing**, 1 ignored doc-test. `cargo clippy --all-targets -- -D warnings` clean.

### Next Session Task

**Add the 6 mandatory v1 indexes (§14.2).** The engine is functional but every lookup is currently O(N) because no index exists beyond the SSTable sort order. Per §17.1, v1 requires:

1. **Entity-by-ID** — already implicit in `(kind=Entity, primary=uuid)` SSTable sort; needs a binary-search block index on top (`<seq>.idx` sidecar, §11.5)
2. **Hyperedge-by-ID** — same shape as #1 for kind=HyperEdge
3. **Lookup-key reverse** — `(external_key_value) → entity_id`. External keys live in metadata hyperedges (§8.1). This is the simplest of the new indexes; recommend starting here.
4. **Adjacency list** — `entity_id → [hyperedge_ids referencing it]`. Critical for "find all approvals for Alice" queries. Probably the highest-value index.
5. **Hyperedge-type clustering** — `type_id → [hyperedge_ids of that type]`. Cheap once you have the adjacency machinery.
6. **Schema-driven property B-tree** — `(type_id, property_id, value) → entity_id`. Hardest of the six; defer if necessary, but try to get the shape right.

**Concrete starting sequence for the next session:**

1. Read this file + the two design docs (especially §14.2 index strategy + §11.5 file layout).
2. Design the **index trait** first — every index implements `update_on_commit(records: &[Record], tx_id: TxId)` and `lookup(query: IndexQuery) -> Vec<EntityId>` (or similar). This trait is what makes new index types pluggable.
3. **Lookup-key reverse index** as the first concrete implementation (simplest, gives an end-to-end pattern). On-disk format: a `<seq>.idx` sidecar with sorted `(external_value_bytes, entity_uuid)` pairs.
4. **Block index** for SSTables — `<seq>.idx` sidecar listing `(start_offset, length, first_key, last_key)` per block. This is what makes existing `SSTableReader::find` O(log N) instead of O(N). Adds a separate "block boundary" concept to the SSTable writer.
5. Adjacency index — a secondary file with `entity_id → [hyperedge refs]` mapping, updated on commit.

**Architectural constraints to remember (locked, do NOT re-litigate):**

- `Engine` is single-writer + `&mut self`. Indexes mutate inside `WriteTxn::commit` after the WAL is durable, before the memtable insert returns.
- Indexes can be **recovered** by replaying records — they're not authoritative. The primary store is the source of truth.
- Index files are NEW file types, sidecars to the `.ndb` files they index. Use `<seq>.idx` (already in §11.5 table) for the SSTable block index; pick new extensions for other indexes (e.g. `<seq>.adj` for adjacency, `<seq>.lookup` for reverse).
- Same atomic-publish pattern: `write-temp + fsync + rename + fsync_dir`.

### After indexes (rough order, for later sessions)

1. **Compaction** — L0 → L1 merge that drops superseded versions and tombstones no live snapshot needs. Needed before storage growth becomes painful.
2. **`Engine::snapshot_iter` performance** — currently rebuilds a `BTreeMap` of all records on every call. Replace with a merge iterator over memtable + SSTable iterators.
3. **Query language parser** — Datalog-influenced surface syntax + JSON AST wire format (§12). Big design task; needs its own focused spec first.
4. **Wire protocol** — HTTP + JSON requests + JSONL streaming responses + embedded mode (§4 architecture overview).
5. **nDB-slicer** companion crate — CPU projection + aggregation (§7, §17.1).
6. **nDB-renderer** companion crate — 2D output (table, scatter, pivot, bar/line/area).
7. **Validation engine** — constraint enforcement from metadata hyperedges (§6.3).
8. **nDB-index-vector-cpu** — HNSW similarity search (§14.2).
9. **nDB-client-rust + nDB-client-python** — wire-protocol clients.
10. **nDB-cli + nDB-mcp-server** — interactive REPL + MCP integration (§17.1).
11. **Arrow IPC interop** — zero-copy bridge to Polars/pandas/DuckDB.
12. **Security baseline** — bearer tokens + TLS + ReBAC capabilities + audit + filesystem encryption (§13).

### Bugs caught + fixed in this session (worth remembering as patterns)

1. **WAL "torn trailing record" must NOT advance pos to `file_len`.** Initial implementation set `pos = file_len` on detecting a partial trailing record, which made `trailing_garbage` always 0 and silently corrupted the truncate-to-safe-boundary recovery flow. Fix: leave pos UNCHANGED on partial detection so `durable_end = pos` lands at the boundary. Caught by tests `truncated_trailing_record_is_treated_as_partial_write`, `truncated_size_prefix_is_partial_write`, `truncate_then_open_append_resumes_at_safe_boundary`.

2. **Memtable `lookup_by_uuid` aggregates 3 buckets.** Entity records and tombstones for the same UUID sort to different `SSTableKey` buckets (because `(kind, primary)` includes the kind byte). Per-key lookup misses the tombstone. Fix: `Memtable::lookup_by_uuid(&uuid, snapshot)` queries Entity + HyperEdge + Tombstone buckets and feeds the union to the visibility resolver.

3. **MANIFEST staleness on restart.** Post-flush commits live in the new WAL but don't bump `manifest.last_tx_id` until the next flush. After close + reopen, `last_tx_id` is the flush-time value, making the replayed records invisible at any snapshot ≤ that watermark. Fix: `replay_wal_into` now returns `(safe_end, max_tx_seen)`, and `Engine::open` reconciles `manifest.last_tx_id` with `max_tx_seen` and persists immediately so the next crash doesn't re-stale it.

### Remaining Acceptance Criteria for v1

These are still open and explicitly punted to future sessions:

- [ ] Vector index algorithm — HNSW vs IVF vs ScaNN (decide in the vector-index commit)
- [ ] Block size + alignment for SSTables (4KB / 16KB / variable) — decide with block index
- [ ] Compression algorithm + block size (Zstd vs LZ4) — decide with block format
- [ ] Query language formal grammar (BNF/EBNF, operator precedence, subquery syntax) — separate focused spec
- [ ] Error-handling specifics (§14.4) — touch up during wire-protocol work
- [ ] Testing strategy specifics (§14.5) — add property tests + crash-injection tests once query/wire layers exist
- [ ] Distribution mechanics — v3+ scope, do not start in v1

### Learnings worth keeping

This session was implementation, not architecture — the design decisions had already been locked in the previous session. The new patterns worth remembering:

- **Write the on-disk decision into the code that implements it**, not just the spec. Every module preamble in this session contains a short "v1 decisions baked in here" block; that's the right level of detail because future readers find it next to the implementation, not in a separate doc.
- **Test the failure modes first** — every storage component has a "torn / corrupted / wrong magic" test alongside the happy-path round-trip. The MANIFEST staleness bug was caught precisely because the end-to-end test exercised "100 records + mid-loop flush + restart", which combined all the corner cases.
- **Sentinel discipline must be enforced symmetrically at encode AND decode.** Encoders reject zero `role_id`/`prop_id`/`type_id` (where forbidden); decoders also reject these sentinels in the same record kinds. Without symmetry, a tampered file could carry illegal values past the parser.
- **CRC-checked envelopes + self-inclusive `record_size` + magic bytes + `format_version` byte** is the standard envelope pattern for every file format in nDB (records, SSTable footer, MANIFEST). Worth lifting into a shared module when the next file format lands; right now there's duplication across `record.rs`, `sstable.rs`, and `db.rs`.

These are reinforcements of patterns already in `~/.claude/rules/programming.md` and `~/.claude/rules/multi-session.md`. No new cross-project rules promoted.
