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
2. **All six secondary indexes on disk** — ✅ DONE.
   - Vector (`.vidx`): brute-force k-NN, so mmap is lossless. 2a format +
     2b/2c write + gather + MVCC re-score by current embedding.
   - Adjacency / type_cluster / entity_type_cluster / lookup_key: a shared
     generic id-list sidecar (`key_bytes → [16-byte id]`, Phase 2d) backs
     all four (Phase 2e). Each query gathers from sidecars + RAM mirror and
     MVCC-verifies via `snapshot_read` (hyperedge live + references entity;
     hyperedge live + of type; entity live + of type; entity live +
     value-matches). `hyperedge_has_type` verifies the record's own type
     directly. Counts = verified-`find` length (exact, O(N) — the low-mem
     speed/RAM tradeoff). `adjacency_overview` stays RAM-mirror-based
     (planner estimate only).
   - **Gating decision:** all sidecars are written only under
     `mmap_indexes` (no default-mode overhead). `Engine::create_with_config`
     added so low-mem DBs emit sidecars during ingest.
3. **Low-memory open + cache.**
   - 3a ✅ O(1) open: `rebuild_indexes` skips SSTables whose every index is
     on disk (`needs_scan`); `reload_constraints_from_metadata` finds
     constraints via `entities_by_type` not a full `snapshot_iter`; a
     `.meta` sidecar preserves tx-timestamps + retention through the skip.
     Without this, `open` re-read the whole DB and the RSS win evaporated
     (measured 1038→339 MB at 0.65 GB).
   - 3b ⏳ Fixed-page LRU buffer pool (mmap→pread) for a HARD cap on the
     inherently-O(N) ops (brute-force kNN reads all embeddings; verified
     counts gather all ids). mmap already gives a reclaimable soft bound;
     3b would make it a guarantee during those scans. Deferred — the big
     read-path rewrite; the 10 GB measure decides whether it's needed.

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

### The ~10 GB test (examples/scale_build, synthetic langgraph corpus)

Real OpenAlex-to-10 GB is infeasible in one session (rate-limited live
fetch). Built a deterministic langgraph-shaped corpus instead — 7.75M papers
with 128-d embeddings + CITES edges, all six indexes registered, **10.08 GB
on disk** — and opened it fresh under `low_memory(2 GiB)`:

```
open: 0.2s | RSS 133 MB            (all six secondary indexes resident: 0.0 MB)
[bounded] property_top_k(10):   647 ms
[bounded] hyperedges_for_entity:  2.4 ms
[bounded] lookup_by_external_key: 0.5 ms
RSS after bounded queries: 453 MB        ← the held-low figure (target was 2–3 GB)
[O(N)] vector_search(k=10): 15.3 s, RSS 2426 MB   (brute-force reads every embedding)
```

**Acceptance met:** a 10 GB nDB opens in 0.2s with RSS 133 MB and serves the
realistic (tile-server) bounded workload at 453 MB — well under 2–3 GB.
The only op that approaches the bound is brute-force k-NN (the engine has no
ANN index): it reads all embeddings (15 s, 2.4 GB resident, still < 3 GB and
mmap-reclaimable). HNSW (or the Phase 3b buffer pool) would bound it; that's
the documented remaining work, not a memory-management failure.

The fix that made this work: the sidecar CRC must cover only metadata
(header + block index), never the bulk — a full-file CRC on open faulted
all ~5 GB of sidecars (open was 13.4s / 4350 MB before the fix).

### Exact / approximate / auto kNN (application layer)

`langgraph-server` gains `--knn exact|approx|auto`:
- **exact** = `engine.vector_search` (on-disk brute-force, bounded RAM, O(N), perfect recall).
- **approx** = in-RAM HNSW (`ndb-index-vector-hnsw`): O(log N), ~95–99% tunable recall.
- **auto** = approx iff the vectors fit the cache budget (`N×(dim*4+~128) ≤ max_cache_bytes/2`), else exact.

**Two structural facts that shaped this:**
1. **In-RAM HNSW costs RAM, doesn't save it.** `instant-distance` keeps every
   vector resident (≈N×dim×4) plus the graph — so approx is the *fast-when-it-fits*
   choice, and `auto` picks it only when the budget covers the vectors (the
   inverse of "big DB → approximate"). Bounded-RAM approximate would need an
   on-disk HNSW (future work).
2. **HNSW is application-layer, not engine.** `ndb-index-vector-hnsw` depends
   on `ndb-engine` (it implements the engine's `Index` trait), so the engine
   can't depend on it — the mode/auto logic lives in the app, matching the
   "generic engine, apps compose" principle. The engine stays exact-only.

Industry context: approximate ANN (HNSW) is the default in every production
vector store (pgvector, Qdrant, Milvus, Weaviate, FAISS, …); exact is reserved
for small or exactness-critical data. So offering both (manual + auto) is the
standard, not a compromise. Verified at 2,500 papers: all three modes serve,
auto resolves correctly, and approx vs exact **recall = 100%@20** (HNSW is
near-exact at this scale; it dips to ~95–99% only at large scale/high dim).

### The langgraph SERVER at scale (lean, low-memory)

`langgraph-server` was refactored to be **uniformly lean** — it no longer
holds an app-side `papers` Vec / adjacency / by-field maps (which were
~1.5–2 GB at millions, the real bottleneck once the engine indexes moved
to disk). It now keeps only the engine + a tiny cluster aggregate
(`<db>/clusters.json`) and serves every `/view` tile from the engine's
on-disk indexes + a bounded `snapshot_read` per returned node.

Measured against a **9M-paper / 5.4 GB** langgraph nDB
(`langgraph-ingest --synthetic 9000000`), opened `--low-memory`:

```
server RSS right after open:            116 MB   (no per-paper RAM, no scan)
+ /view/clusters + a /view/top:         337 MB
+ /view/neighbors (bounded BFS):        337 MB   (no growth)
+ brute-force /view/knn (all embeds):  ~1057 MB  (reclaimable; < target)
```

Before the refactor the server needed ~2 GB *just to start*. Now open is
116 MB and the realistic bounded workload sits ~337 MB — under the 2–3 GB
target. (16-d demo embeddings cap the synthetic corpus at 5.4 GB for 9M
papers; the engine itself was proven at the full 10.08 GB above. Server RSS
is scale-independent at open, so corpus size doesn't move it.) Two O(N)
ops remain: brute-force kNN (no HNSW) and `property_top_k` across many
uncompacted sidecars (each `top_k` reverse-scans its whole bucket — a
compaction pass would collapse the 22 sidecars to one and bound it).

### Phase 2 measured result (same example, 100k entities)

```
default   indexes 77.1 MB [lk 5.3 adj 31.3 tc 14.9 etc 14.9 vec 5.3 pbt 5.3]
lowmemory indexes 66.4 MB [lk 5.3 adj 31.3 tc 14.9 etc 14.9 vec 0.0 pbt 0.0]
```

Both `pbt` and `vec` now served from disk. The example uses 16-d
embeddings (so `vec` is modest here); at real embedding sizes the vector
index dominates resident RAM, making this the decisive 10 GB lever.
Remaining in-RAM: `adj`/`tc`/`etc`/`lk` — the next Phase 2 sidecars.
