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
| Contention-free (off-lock) compaction | **Partial** | scheduling landed; off-lock merge is the follow-on |
| LSM write-stall / L0 backpressure | **Planned** | engine concurrency surgery; see below |

The server limits land via a `ServerConfig` (builder methods; existing
constructors unchanged) — defaults: `max_connections=256`, read/write
timeout `30s`, `max_header_bytes=64 KiB` (→`431`), `max_body_bytes=16 MiB`
(→`413`, enforced pre-read on `Content-Length` so a malicious length can't
OOM). Bounded concurrency uses a CAS-acquired RAII `ConnGuard` over an
`AtomicUsize` so two acceptors can't race past the cap; the plain and TLS
accept paths are now both thread-per-connection and shutdown-aware. 42
ndb-server tests pass (10 new), clippy-clean.

**Automatic compaction** is now landed: `CompactionPolicy { l0_trigger,
check_interval }` + `SharedEngine::spawn_auto_compactor` run a named
background thread that compacts when the live-SSTable count hits the
trigger (default 4), with a stoppable `CompactorHandle`. This closes the
"operator must call `compact()` by hand" gap. It **schedules** compaction;
it does not yet make it contention-free — the compaction still holds the
engine write lock for its duration. The follow-on (off-lock merge: read the
immutable input SSTables by path, take the lock only for the manifest swap)
plus a bounded-memtable **write-stall backpressure** signal are the
remaining concurrency work — deferred deliberately rather than shipping a
version that can dead-lock or corrupt under load.

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
primitives, **automatic background compaction** (engine) and bounded
concurrency + timeouts + request limits + `/metrics` + `/ready` + graceful
shutdown (server). Deliberately deferred — because a half-correct version is
worse than a documented gap — block compression (needs a dependency +
block-format work), **off-lock contention-free compaction** + LSM
write-stall backpressure (single-writer concurrency surgery where a bug is
data corruption), and the replication network daemon (server wiring). The
deferred items are design-clear, not blocked.
