# ADR: Low-RAM core — on-disk secondary indexes + bounded block cache (Option B)

Status: **accepted** (decision scored last session: A mmap-only = 7, **B = 10**, C scan-fallback = 5).
Date: 2026-05-29. Branch: `main`.

## Problem

`Engine::open` calls `rebuild_indexes()` (engine.rs ~L490), which scans every
SSTable + the memtable and rebuilds all six secondary indexes
(`lookup_key`, `adjacency`, `type_cluster`, `entity_type_cluster`,
`vector`, `property_btree`) **fully resident in RAM**. These grow linearly
with the data, so a 10 GB nDB needs many GB of RAM just for indexes —
independent of the data SSTables, which are already `mmap`'d (OS-paged,
low RAM) and have a `.idx` block-index sidecar for O(log N) primary-key
reads.

Goal: a **core** config that caps resident RAM at ~2–3 GB regardless of
DB size, without gambling the MVCC read path or breaking existing DBs.

## Decision: Option B

Persist each secondary index to disk (built at flush/compaction like data
SSTables), `mmap` it, and govern resident memory with a bounded block
cache controlled by a new `EngineConfig`. Reuses the existing
SSTable + block-index-sidecar + mmap machinery. An **extension**, not a
rewrite.

## Config surface (Phase 0)

```rust
pub struct EngineConfig {
    /// Hard ceiling for the Phase-3 block cache, in bytes. Default 2 GiB.
    pub max_cache_bytes: usize,
    /// Serve on-disk-capable secondary indexes from mmap'd sidecars
    /// instead of rebuilding them in RAM on open. Default `false`
    /// (= today's behaviour, full back-compat). `low_memory` forces true.
    pub mmap_indexes: bool,
    /// Convenience preset: implies `mmap_indexes = true` and a tighter
    /// cache budget. Default `false`.
    pub low_memory: bool,
}
```

- `Engine::open(path)` == `Engine::open_with_config(path, EngineConfig::default())`
  — **behaviour unchanged** (rebuilds in RAM). Back-compat guaranteed.
- `Engine::open_with_config(path, cfg)` is the new opt-in entry point.
- The config is stored on `Engine` and consulted by `open`, the query
  paths, and (Phase 3) the block cache.

## Instrumentation (Phase 0)

`Engine::index_memory_stats() -> IndexMemoryStats` returns a per-index
resident-byte estimate (each index gains a `heap_bytes()` method). Lets
us record the **RAM-vs-DB-size curve** we are driving down, before/after
each phase, in commit bodies.

## On-disk index format — per-SSTable sidecar (Phases 1–2)

Mirror the `.idx` block-index sidecar exactly: one immutable index file
per SSTable, written atomically (temp→rename→fsync) at flush/compaction,
`mmap`'d on open, graceful fallback to RAM rebuild if missing/corrupt.

### Phase 1 — property index sidecar `<seq>.pidx`

A sorted, immutable, block-indexed map
`(TypeId, PropertyId, value_bytes) → [EntityId]`, covering exactly the
records in its sibling `<seq>.ndb` SSTable, for the registered
`(type, prop)` pairs.

```text
header                              16 bytes
  magic            4 = b"NDPX"
  format_version   u8  (1)
  reserved         u8 [3]
  block_size       u32
  entry_count      u32
entries  (sorted ascending by (type,prop,value_bytes))
  per entry:
    key_len        u16
    key_bytes      key_len   = type(4 LE) | prop(4 LE) | value_index_bytes
    entity_count   u32
    entity_ids     16 × entity_count
block-index (over entry keys, same shape as .idx)  — seek to a key region
trailer crc32       u32   over header+entries+block-index
```

`value_index_bytes` reuses `property_btree::value_to_index_bytes`
(order-preserving big-endian encoding) so on-disk byte order == numeric
order → range/top_k are contiguous scans.

Operations served from the mmap'd file:
- `find(type,prop,value)`  — block-index seek + linear scan to exact key.
- `range(type,prop,lo,hi)` — seek to `lo`, forward-scan to `hi`.
- `top_k(type,prop,k)`     — seek to bucket end, **reverse**-scan.

### MVCC correctness (the non-shortcut)

A per-SSTable sidecar only knows values *as of its own SSTable*. An entity
updated in a newer SSTable would otherwise appear under its stale value
from an older one. So a query that opts into mmap indexes:

1. Gathers candidate `EntityId`s from **all** `.pidx` sidecars + the
   in-RAM memtable property index (the memtable is bounded by the flush
   threshold, so its RAM portion is small and constant).
2. **Verifies** each candidate against the current snapshot via
   `snapshot_read` — drops tombstoned entities, superseded versions, and
   entities whose *current* value no longer matches the predicate.

This is the standard LSM read path (gather + resolve), not an
MVCC shortcut. `top_k` k-way-merges the per-sidecar reverse streams and
verifies until `k` survivors are confirmed — bounded by `k` + stale
skips. Compaction collapses versions, shrinking the stale set over time.

Resident RAM for the property index then = only the memtable's portion
(bounded) + mmap'd sidecars (OS-paged). On `open` with
`mmap_indexes=true`, `property_btree` is **not** rebuilt from SSTables.

### Phase 2 — remaining indexes

Same sidecar pattern, in order of RAM payoff:
`entity_type_cluster`, `type_cluster`, `adjacency`, `lookup_key`, then the
`vector` index (mmap'd HNSW / flat vectors with acceptable recall). After
each, `open` no longer rebuilds it in RAM; open cost trends toward O(1).

## Phase 3 — bounded block cache (the hard cap)

A fixed-size LRU buffer pool over fixed-size pages sitting in front of
index + data page reads, honoring `config.max_cache_bytes`. `mmap` gives
a *soft* (OS-page) bound; this makes "use 2–3 GB" a *guarantee* at any DB
size. Pure-function LRU core is TDD'd.

## Back-compat & versioning

- New on-disk files carry their own magic + `format_version`; readers
  reject newer versions and fall back to RAM rebuild on missing/corrupt
  sidecars (same policy as `.idx`).
- No existing on-disk format changes. Databases written before this work
  open unchanged (no sidecars → `mmap_indexes` silently rebuilds in RAM
  for the missing ones).
- Default `Engine::open` is byte-for-byte the old path.

## Test gate

`cargo test --workspace -- --test-threads=1` (526 tests) green after
**every** commit. Pure encode/decode + LRU are TDD'd; format/IO glue is
verified end-to-end.

## Phases / commit plan

0. ✅ Config + `open_with_config` + per-index `heap_bytes()` + baseline.
1. ✅ `.pidx` property index on disk; served from mmap; gather+verify; not
   rebuilt in RAM under `mmap_indexes`. The proof.
   - 1a ✅ on-disk file format (`property_index_file.rs`, 15 TDD tests).
   - 1b ✅ write `.pidx` at flush + compaction; delete with the SSTable.
   - 1c ✅ read path under `mmap_indexes`: open sidecars, skip RAM rebuild
     of sidecar-backed property data, gather candidates from sidecars +
     the memtable mirror, MVCC-verify against the latest snapshot. The RAM
     property mirror holds only memtable + sidecar-less data.
2. ⏳ Remaining indexes on disk, in RAM-payoff order: **vector first**
   (embeddings dominate resident RAM — the 10 GB win hinges on this),
   then entity_type_cluster, type_cluster, adjacency, lookup_key.
3. ⏳ Bounded block cache enforcing `max_cache_bytes` (hard cap).

Then: wire `langgraph-server` to `open_with_config(low_memory(..))`, build a
real ~10 GB LangGraph nDB, verify RSS held ~2–3 GB with bounded query
latency, recorded.

**Note on the 10 GB RSS target:** Phase 1 bounds only the property index.
The vector + adjacency indexes still rebuild in RAM, so the "RSS ≤ 3 GB at
10 GB" acceptance needs Phase 2 (especially the vector index). Phase 1 is
the validated proof of the mechanism on one index.

### Phase 1 measured result (examples/index_mem_baseline.rs)

The property B-tree component of resident RAM drops to the memtable-only
mirror (≈0 in steady state) under `low_memory`, served instead from mmap'd
`.pidx` files — while find/range/top_k return identical results to the
default in-RAM path (verified by `low_memory_query_matches_default` +
MVCC update/tombstone tests).
