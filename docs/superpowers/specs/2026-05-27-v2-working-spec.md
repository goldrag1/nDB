# nDB v2.0 — Working Spec

> **Status:** Drafted 2026-05-27, closes immediately after v1.3.0 ships.
> Locks the v2.0 release identity, scope, sequencing, and success
> criteria. Drives the next several sprints of work.

## 1. Identity

**v2.0 is the "polish v1" release.** Every v1 documented limitation that
ships as a `// v2 polish` caveat in the code becomes a real implementation.
The user-facing surface stays compatible — v1.3 clients work against v2.0
without changes. Internal data formats may evolve; migration is handled
on `Engine::open` via in-place rewrite or one-shot conversion.

This is **NOT** a platform release. The shape of nDB doesn't change —
single-database, mostly-single-process, single-writer-by-default. New
shape work (distributed mode, write-via-query, gRPC, JS/Go clients) is
explicitly deferred to v3.

One Theme-B slice creeps in: **concurrent-writer relaxation**. v1.3 had
`&mut Engine` on `begin_write` which serialized writes at the type level.
v2.0 changes this to internal RwLock-based serialization so multiple
concurrent writers can queue without type-system gymnastics. This unlocks
the most common multi-tenant deployment pattern (one Engine, many
short-lived writers) without requiring true distributed consensus.

## 2. Scope — locked deliverables

### Sprint 1 — Random-read perf + crash-free restart (2-3 weeks)

These items don't depend on each other and parallelize well across the
two halves of a sprint.

#### 2.1 Block index sidecar (`<seq>.idx`)

Goal: `SSTableReader::find` becomes O(log N) instead of O(N).

Design:
- For each `<seq>.ndb` SSTable, emit a sidecar `<seq>.idx` containing
  a sorted list of `(SSTableKey, byte_offset)` pairs at fixed intervals
  (block boundaries every ~4 KiB of data).
- `find()` binary-searches the sidecar to find the block, then
  linear-scans within the block. Worst case = one block read = 4 KiB.
- Sidecar is mmap'd at reader open time. Size scales with N at
  ~32 bytes per block ≈ 0.8% of SSTable size.
- MANIFEST gets a flag per SSTable entry indicating whether the sidecar
  exists; engines that find missing sidecars fall back to linear scan
  (forward compatible with v1.3 databases).
- Crash recovery: sidecar is written AFTER the main `.ndb` via the
  same write-temp-then-rename pattern. Missing sidecar = treat as v1.3
  SSTable.

Tests:
- Round-trip with 100k records; assert binary search returns same
  result as linear scan.
- Sidecar missing → graceful fallback to linear scan.
- Sidecar corrupted (bad CRC) → engine refuses to open the sidecar,
  emits a warning, falls back to linear scan.

Effort: 1-2 weeks. Single-author.

#### 2.2 Persisted commit timestamps + retention policies

Goal: `tx_at_or_before(ts)` and `retention_policy(type_id)` survive
engine restart.

Design:
- Two new record kinds: `TxTimestampRecord` (kind 0x07) and
  `RetentionPolicyRecord` (kind 0x08).
- `TxTimestampRecord`: `record_size, kind, format_version, tx_id, timestamp_us, crc32`. Written every commit as the FIRST record in the WAL batch.
- `RetentionPolicyRecord`: `record_size, kind, format_version, type_id, policy_kind, keep_last_n, crc32`. Written when `set_retention_policy` is called.
- WAL replay populates `commit_timestamps` map; SSTables also retain
  these records for cross-restart durability.
- Compactor preserves the most-recent `RetentionPolicyRecord` per
  type_id; old ones can be dropped.

Tests:
- Set retention, commit some records, close, reopen → retention still
  active.
- Record commits, close, reopen → `as of "<rfc3339>"` finds the right
  tx_ids.
- Crash recovery: WAL truncation at a `TxTimestampRecord` boundary
  doesn't corrupt the chain.

Effort: 3-5 days.

#### 2.3 Engine-side lazy iterator pipeline

Goal: `Engine::snapshot_iter` returns an iterator that produces records
lazily (no materialised `Vec<Record>`); `/iter` and `/query_stream` truly
stream without memory ceiling.

Design:
- New method `Engine::snapshot_iter_streaming(snapshot) -> impl Iterator<Item = Result<Record, EngineError>>` that uses a k-way merge across memtable + SSTable iterators.
- Each SSTable already yields records in `(kind, primary)` order; merge
  is a `BinaryHeap` of `(SSTableKey, source_index)`.
- Per-key version resolution happens inline: collect all versions
  for the current key (peek next-source repeatedly while equal),
  resolve, emit visible winner.
- `snapshot_iter` (returns `Vec`) stays for backward compat — internally
  collects from the streaming variant.
- Query executor rewrites to consume the streaming iterator; `/query`
  and `/query_stream` no longer materialise.

Tests:
- 1M-record streaming iter — assert peak memory stays well below 1M ×
  record size.
- Result-set ordering identical to the materialised version.
- Early termination (e.g., `.take(10)`) closes iterators promptly.

Effort: 1 week.

### Sprint 2 — Concurrency + compaction (3-4 weeks)

#### 2.4 Concurrent-writer relaxation (RwLock-based engine)

Goal: Multiple writers can be opened concurrently against a single
engine. They serialize internally via RwLock; from the API standpoint
each `begin_write` returns immediately or briefly blocks behind the
prior writer's commit.

Design:
- `Engine` field signatures change: most mutable state moves into
  `RwLock<EngineState>`. Read paths take `read()` locks; commits take
  `write()`.
- `Engine::begin_write` changes from `&mut self` to `&self`. The
  returned `WriteTxn` holds a `MutexGuard` (or write lock) preventing
  other writers from progressing concurrently. Readers continue to
  hold read locks and serve queries.
- Wire protocol unchanged.
- Server stops needing `Mutex<Engine>` — passes `Arc<Engine>` instead.
- MCP server, CLI, client crates need a recompile but no API changes.

Tests:
- Spawn N writer threads each doing 100 commits; verify total commit
  count = N × 100 and tx_ids are monotone unique.
- Reader thread runs concurrently with writers; all queries succeed,
  none see partial states.
- Deadlock-free across a stress harness.

Effort: 2-3 weeks. The hardest item in v2.0 because every internal
mutation point needs review.

#### 2.5 Snapshot-aware compaction

Goal: Compaction never drops a version that an active read transaction
might need.

Depends on: 2.4 (so we can track active read snapshots).

Design:
- `Engine::register_active_snapshot(tx_id)` / `release_active_snapshot(tx_id)` — readers track their snapshot.
- Compactor reads "oldest live snapshot" = min over active set + last
  successful compaction watermark.
- For each version, only drop if `tx_id_supersede < oldest_live`. Same
  rule for tombstones.
- Snapshot register backs a `BTreeMap<TxId, usize>` (count of active
  readers per tx).

Tests:
- Start a long-running iter, then commit + compact; verify iter still
  sees the original snapshot.
- Compactor stats include `oldest_live_snapshot` and `versions_kept_for_snapshot`.

Effort: 1 week.

### Sprint 3 — Query planner + subscribe + encryption (1.5-2 weeks)

#### 2.6 Cardinality-aware query planner

Goal: Pattern execution order is picked by estimated cardinality, not
source order.

Design:
- New module `ndb-engine::query::plan` with explicit `Plan` AST.
- Per-atom cardinality estimator using:
  - `property_btree.count(type_id, property_id, value)` for B-tree filters
  - `type_cluster.count(type_id)` for full hyperedge-type scans
  - `adjacency.degree(entity_id)` for adjacency-bound patterns
  - Heuristic estimate for recursive patterns (`avg_degree × depth_cap`)
- Greedy: pick lowest-cardinality atom as seed; subsequent atoms
  picked by `max_shared_vars` then `min_cardinality` tiebreak.
- `EXPLAIN`-style trace available for debugging (new `Engine::explain_query` method).

Tests:
- Hand-built queries where source-order chooses worst; cardinality-aware
  picks best; same result set, much lower work.
- EXPLAIN output is stable + readable.

Effort: 3-5 days.

#### 2.7 Condvar-based subscribe

Goal: `/subscribe` returns the moment a commit happens, no polling.

Design:
- `Engine` gains `commit_notify: Arc<(Mutex<TxId>, Condvar)>`.
- `WriteTxn::commit` calls `condvar.notify_all()` after success.
- `Engine::wait_for_commit(since: TxId, timeout: Duration) -> Option<TxId>` blocks on the condvar with a timeout-based `wait_timeout_while`.
- Server's `handle_subscribe` uses this instead of 50ms polling.

Tests:
- Subscribe blocked, commit fires, subscribe returns within <10ms.
- Timeout still hits if no commit.
- Multiple subscribers all wake on a single commit (no thundering herd
  problem since notify_all is intentional here).

Effort: 2-3 days.

#### 2.8 WAL + SSTable encryption wiring

Goal: When `NDB_ENC_KEY` is set, WAL and SSTable bytes on disk are
AES-GCM encrypted.

Design:
- `Engine::create` and `open` consult the key source (env or
  `KeyProvider` trait). If a key is present and the database has no
  encryption marker, refuse to open until the user explicitly migrates.
- `<db>/.encryption` marker file containing the algorithm + chunk size
  + key fingerprint. Set on first encrypted commit.
- `WriteAheadLog` writes via `Cipher::encrypt_chunked` (already shipped
  as a primitive in v1.0). Each record is its own chunk.
- `SSTableWriter` encrypts the whole file as one chunked stream.
  `SSTableReader::open` notices the encryption marker and decrypts on
  read.
- Mmap still works — decrypt lazily into a `Cow<[u8]>` buffer per
  block. Or store a decrypted overlay per SSTable.

Tests:
- Encrypted commit + read round-trips correctly.
- Wrong key → engine refuses to open with `EncryptionKeyMismatch`.
- Mixed encrypted/unencrypted SSTables in the same DB are rejected
  (one encryption mode per DB).

Effort: 1-2 weeks.

#### 2.9 Capability hyperedges as persistent ReBAC

Goal: Move `principals.json` into the database as capability
hyperedges.

Design:
- Reserved type `TYPE_CAPABILITY` and reserved roles `subject`,
  `action`, `target`, `granted_at`, `expires_at`.
- Auth lookup: `engine.has_capability(principal, action, target, now)`
  walks capability hyperedges incident on the principal entity.
- Server's `Principals` struct migrates from in-memory map to a
  thin wrapper around engine queries.
- v1.3 `principals.json` is still supported as a bootstrap import
  source: on Engine::open, if the file exists and no capability
  hyperedges are in the database, import them (one-shot).

Tests:
- Token-based auth still works after migration.
- New capability committed via `/commit` is immediately effective on
  the next request.
- Expired capability is rejected.

Effort: 1 week.

## 3. Out of scope — v3 territory

These DO NOT ship in v2.0; defer to a future v3 working spec:

- True distributed mode (read replicas, geo-replication, raft consensus)
- Write-via-query (extending §12 grammar with mutations)
- IVF / ScaNN vector indexes (HNSW is enough for v2-scale workloads)
- gRPC alternative transport
- JS/TS and Go client crates (Python + Rust serve the target audience for v2)
- Pilot deployment integration (Frappe connector, etc.) — separate
  project once v2.0 ships

## 4. Sequencing rationale

| Sprint | Deliverables | Effort | Cumulative |
|---|---|---|---|
| 1 | Block index, persisted ts/retention, lazy iter | 2-3 wk | 3 wk |
| 2 | Concurrent writers, snapshot-aware compaction | 3-4 wk | 6-7 wk |
| 3 | Planner, subscribe, encryption wiring, ReBAC | 1.5-2 wk | 8-9 wk |

**Critical path:** 2.4 (concurrent writers) gates 2.5 (snapshot-aware
compaction). Everything else can parallelize.

**Earliest beta:** end of Sprint 2 if 2.6–2.9 slip into a 2.1 release.

**Most conservative ship date:** ~10 weeks from work start, allowing for
debug + benchmark + docs + release notes.

## 5. Success criteria (gates for v2.0 release)

1. **Performance regression test ≤ 0% on every benchmark.** Existing
   `ndb-bench` simple + biology workloads must not slow down on a 10k
   dataset.
2. **Cold-start large-DB read 10×+ faster than v1.3** on a 1M-record
   database, due to block index + lazy iter.
3. **Subscribe latency ≤ 1ms p99** under low load (vs ~50ms v1.3 baseline).
4. **All v1.3 tests still pass.** Plus 100+ new tests for v2 features.
5. **Clippy clean** with `-D warnings`.
6. **Engine opens any v1.3 database without conversion.** Forward-only
   migration — v2.0 reads v1.3 files; v2.0 writes new files; mixed-mode
   readability documented.

## 6. Open questions (locked before sprint 1 starts)

- **Concurrent-writer cardinality target.** 2-4 concurrent writers
  (workspace-style) or hundreds (server-style)? Affects whether we
  use a single global RwLock (fine for 2-4) or fine-grained per-index
  locks (needed for hundreds). v1 polish ≠ massive concurrency, so
  start with global lock; benchmark; only escalate if there's measurable
  contention.

- **Block index density.** Every 4 KiB vs every 32 KiB? Smaller blocks
  → bigger sidecar, faster lookups; larger blocks → smaller sidecar,
  slower lookups (still O(log N) with constant-factor differences). Pick
  4 KiB as the default; expose as a tuning knob.

- **Encryption migration.** Force re-write of all SSTables on switch
  from unencrypted to encrypted, or accept mixed-mode? Decision: force
  full re-write via `Engine::reencrypt(new_key) -> Result<_>`. Mixed
  mode is too error-prone.

- **Retention persistence format.** New record kind 0x08 vs MANIFEST
  extension? Decision: new record kind. Keeps MANIFEST shape stable;
  retention policies are content, not metadata about the file layout.

## 7. v2.0 release readiness

When all 9 deliverables ship + success criteria pass:
- Tag `v2.0.0`
- Write `2026-XX-XX-v3-working-spec.md` covering distributed mode
  + write-via-query
- Update README to reflect v2 capabilities
- Frappe pilot deployment can begin (using v2.0 as the engine)
