# nDB Production-Readiness

Status of the hardening pass that moves nDB from "correct embedded engine"
toward "operable production database". Tiers are P0 (correctness/durability)
→ P3 (high availability). Each item is marked **Landed**, **Partial**, or
**Planned** with the commit or the reason it's deferred — no silent gaps.

## P0 — Correctness & durability

| Item | Status | Where |
|---|---|---|
| WAL group-commit + fsync, replay on restart | Landed (pre-existing) | `wal.rs` |
| Per-record + footer + sidecar CRC32 | Landed (pre-existing) | `sstable.rs`, `block_index.rs` |
| AES-256-GCM at rest | Landed (pre-existing) | `encryption.rs` |
| **Bloom filter sidecar** (cut read amplification) | **Landed** | `bloom.rs` |
| **Decoder fuzz/robustness suite** (no panic on hostile bytes) | **Landed** | `tests/robustness.rs` |
| Block compression (zstd/lz4) | **Planned** | needs a dep + block-format change; see below |

**Bloom filters.** Every SSTable now carries a `.bloom` membership summary.
A point read consults it first and returns immediately when the key is
provably absent — skipping the block-index search and in-block scan for the
common miss case. Pure Rust (FNV-1a double hashing, no new dep), 1% target
FPR, never a false negative (the sidecar is only emitted when it covers
every record; a missing/corrupt one is simply ignored). This is the single
biggest lever on the "10 GB explorer feels slow" problem.

**Robustness.** A deterministic fuzz-lite suite drives `Record::decode`,
`Value::decode`, the peek helpers, the bloom/block-index loaders, and
`SSTableReader::open` with random, truncated, bit-flipped, and
trailing-garbage input. The contract: malformed bytes always surface as a
clean `Err`, never a panic/hang/OOB. These parsers face the network and
possibly-corrupt disk, so this is load-bearing.

## P1 — Concurrency & resource limits (server)

| Item | Status | Where |
|---|---|---|
| Bounded concurrent connections + reject over cap | Landed | `ndb-server` |
| Thread-per-connection on both plain + TLS paths | Landed | `ndb-server` |
| Per-connection read/write timeouts | Landed | `ndb-server` |
| Request line/header/body size limits (413/431) | Landed | `ndb-server` |
| **Automatic background compaction** (policy + thread) | **Landed** | `shared.rs` |
| **Contention-free (off-lock) compaction** | **Landed** | `shared.rs`, `engine.rs` |
| **Server `/compact` is off-lock** | **Landed** | `ndb-server` via `run_offlock_compaction` |
| **LSM write-stall backpressure + memtable auto-flush** | **Landed** | `engine.rs`, `shared.rs`, `ndb-server` |
| **No index rebuild at compaction install** (O(total)→O(1)) | **Landed** | `engine.rs` |

The server limits land via a `ServerConfig` (builder methods; existing
constructors unchanged) — defaults: `max_connections=256`, read/write
timeout `30s`, `max_header_bytes=64 KiB` (→`431`), `max_body_bytes=16 MiB`
(→`413`, enforced pre-read on `Content-Length` so a malicious length can't
OOM). Bounded concurrency uses a CAS-acquired RAII `ConnGuard` over an
`AtomicUsize` so two acceptors can't race past the cap; the plain and TLS
accept paths are now both thread-per-connection and shutdown-aware. 42
ndb-server tests pass (10 new), clippy-clean.

**Automatic compaction** is landed: `CompactionPolicy { l0_trigger,
check_interval }` + `SharedEngine::spawn_auto_compactor` run a named
background thread that compacts when the live-SSTable count hits the
trigger (default 4), with a stoppable `CompactorHandle` — closing the
"operator must call `compact()` by hand" gap.

**Off-lock (contention-free) compaction** is now landed too. Compaction is
split into three phases: `Engine::plan_compaction` (locked, brief —
snapshot the input set + reserve an output seq), `merge_planned` (OFF-LOCK,
long — reopen the immutable inputs by path, merge, write the output), and
`Engine::install_planned_compaction` (locked, brief — a **set-based**
manifest swap that removes exactly the planned inputs and keeps any SSTable
flushed while the merge ran). `SharedEngine::compact` routes through this,
so the background compactor is contention-free for free. Safety: compactions
are serialised by a dedicated mutex (no overlapping input sets) without
blocking reads/writes; the snapshot floor is re-checked under the install
lock and the run aborts (discarding its output) if a reader registered an
older snapshot mid-merge; and the newest-first SSTable ordering that drives
MVCC resolution flows through a single shared helper used by both `open` and
install. Proven by a deterministic set-swap test + a multi-threaded
writer-vs-compactor no-data-loss test, on top of every existing compaction
test now exercising the off-lock path.

The HTTP server's `/compact` is now off-lock too — but via the cleaner route
rather than a full `SharedEngine` swap. Swapping the server's
`Arc<RwLock<Engine>>` for `Arc<SharedEngine>` would have changed the
`engine()` accessor's return type and cascaded into `ndb-mcp-server`, so
instead the orchestration was extracted into a reusable
`run_offlock_compaction(&RwLock<Engine>, &Mutex<()>, floor, current_floor)`
free function that both `SharedEngine::compact_offlock` and the server call.
The server passes `TxId::ACTIVE` for the floor: it registers no read
snapshots and every read handler is a single lock acquisition, so the atomic
install swap alone keeps reads consistent. The handler documents the standing
contract — any future read that pins an old snapshot across multiple lock
acquisitions must enroll it in a snapshot registry first.

**Write-stall backpressure + memtable auto-flush** complete the concurrency
story. Two opt-in `EngineConfig` knobs (both default `0` = disabled, so
behaviour is unchanged): `memtable_flush_threshold_bytes` triggers
`Engine::auto_flush_if_full` (the primary resident-write-memory bound), and
`l0_stall_threshold` makes `Engine::check_write_admission` return
`WriteStalled` once the live SSTable count shows flushes outpacing
compaction. `EngineConfig::from_env` (`NDB_MEMTABLE_FLUSH_BYTES` /
`NDB_L0_STALL`) wires them through the env-sourced server/CLI constructors,
and the server's `/commit` maps `WriteStalled → 503` + auto-flushes after a
commit. **Crucially the stall is a rejection, not a block**: a blocking write
would hold the engine write lock waiting for compaction and dead-lock the
off-lock compactor's install phase — proven safe by a multi-threaded
writer-vs-compactor test that the stall never deadlocks.

The last big lock-hold in compaction — the index rebuild at install — is now
gone too. It turned out not to need an incremental-update algorithm at all:
the in-memory indexes are **invariant under compaction**. Every index applies
records with a self-cleaning, order-independent rule (track `latest_tx`,
ignore older records, remove a key's prior entries before re-inserting), so
the index state is a pure function of each key's latest version — and
compaction only drops superseded/tombstoned records, never the visible
winner. So the rebuild was redundant: removed, not replaced. Default-mode
install index cost goes O(total) → O(1). A test snapshots the full query
surface after a compaction (superseded value + tombstone), forces an explicit
rebuild, and asserts the two are identical — in both default and mmap mode.

## P2 — Observability & operability

| Item | Status | Where |
|---|---|---|
| `GET /metrics` Prometheus endpoint (hand-rolled, no dep) | Landed | `ndb-server` |
| `GET /ready` vs `/health` split | Landed | `ndb-server` |
| Graceful shutdown + `POST /admin/shutdown` (Admin cap) | Landed | `ndb-server` |
| **Consistent hot backup** (`Engine::backup_to`) | **Landed** | `engine.rs` |
| Engine-internal metrics (compactions, flushes, bloom skips) | **Planned** | overlaps server `/metrics`; wire post-merge |

Metric series exposed: `ndb_requests_total{route}`, `ndb_responses_total
{status}`, `ndb_connections_in_flight`, `ndb_connections_rejected_total`,
`ndb_request_duration_seconds_sum`/`_count`, `ndb_bytes_read_total`,
`ndb_bytes_written_total`. `/ready` returns `{"status":"ready",
"last_tx_id":N}` on a cheap manifest read, `503` while draining or if the
engine lock is poisoned. Shutdown keeps accepting during a bounded drain
window so an orchestrator's `/ready` can flip to `503` before the listener
closes — graceful from the load-balancer's point of view.

**Hot backup.** `Engine::backup_to(dest)` takes a point-in-time image while
the engine stays open: it copies every manifest-referenced SSTable (immutable
after publish, so safe under concurrent writes) plus the active WAL, so the
backup captures all *committed* state including records not yet flushed.
Restore is just `Engine::open(dest)`; a torn WAL tail recovers exactly as a
crash would. This is also the bootstrap step for replication (below).

## P3 — High availability

| Item | Status | Where |
|---|---|---|
| **Log-shipping replication primitives** | **Landed** | `replication.rs` |
| Network hop (leader `/replicate`, follower poll) | **Planned** | server wiring |
| Continuous cross-WAL-rotation cursor | **Planned** | follower re-syncs sealed SSTables via backup today |
| Raft consensus | **Planned** | big lift; defer until a design partner needs it |

**Replication.** nDB replicates the PostgreSQL way: a base backup bootstraps
a follower, then `read_wal_since(wal, cipher, after)` (leader CDC) streams
committed records and `apply_batch(follower_wal, batch)` appends them to the
follower's WAL. It's correct by construction — `commit()` writes records to
the WAL without re-stamping tx ids and the WAL layer re-encodes them
verbatim, so a shipped record carries the leader's original
`tx_id_assert`/`tx_id_supersede` + `TxTimestamp`. The follower only appends
bytes the leader already made durable and reconstructs state via the standard
crash-recovery path — there is no second apply path to get wrong, and replica
MVCC is byte-identical to the leader's. Watermarks are WAL byte offsets, so a
follower's WAL length is its resume point. What remains is the network daemon
(a leader endpoint + follower poll loop) and a cursor that spans WAL rotation
without a base-backup re-sync.

## Honest summary

Landed this pass: bloom filters, decoder fuzzing, hot backup, replication
primitives, automatic background compaction, **off-lock contention-free
compaction** (engine) and bounded concurrency + timeouts + request limits +
`/metrics` + `/ready` + graceful shutdown (server). Deliberately deferred —
because a half-correct version is worse than a documented gap — block
compression (needs a dependency + block-format work) and the replication
network daemon. The deferred items are design-clear, not blocked.
