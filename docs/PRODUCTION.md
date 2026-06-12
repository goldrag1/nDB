# Running nDB in production

What the engine guarantees today, how to operate it, and what is **not**
yet automated. Grounded in the shipped code — every "✅" item has tests.

## Durability & crash recovery ✅

- Commits append to a **`fsync_data`'d WAL** before acknowledging; a crash
  replays the WAL into a fresh memtable on next open (same code path used
  by replication). Multi-record batches use a single grouped `fsync`, so a
  commit is all-or-nothing across a crash.
- SSTables and their sidecars carry CRC32; the decoders reject malformed
  input cleanly (bounded allocations — see the robustness suite) rather
  than panicking or OOM-ing.
- At-rest **AES-GCM-256 encryption** via `NDB_ENC_KEY` (opt-in; refuses to
  open on key-fingerprint mismatch).

## Concurrency model ✅

- **Single writer, concurrent readers.** The engine is single-writer by
  type (`begin_write` takes `&mut`); `ndb-server` wraps it in
  `Arc<RwLock<Engine>>` so reads parallelise and the writer takes the
  exclusive slot. MVCC + SSI provides snapshot isolation.
- Validated by `crates/ndb-engine/tests/concurrency.rs`: 6 reader threads
  scanning while a writer commits never observe a half-applied commit, and
  the final state equals exactly what was committed.
- **Capacity implication:** write throughput is bounded by the single
  writer; reads scale with cores. Plan accordingly — this is a
  read-scaling, strongly-consistent store, not a multi-master one.

## Compaction ✅

Automatic: a background thread merges SSTables once the open count crosses
a threshold (default 8, checked every 30s), using the off-lock path so
commits/reads keep running during the merge. Observable via
`ndb_auto_compactions_total` in `/metrics`. Tune with
`Server::with_auto_compaction(threshold, interval)`; `0` disables it (then
use `POST /compact`).

## Server hardening ✅

Connection cap (`503` past it), per-connection read/write timeouts, request
header/body size caps, graceful shutdown with a bounded drain window, and
`/health` (liveness) + `/ready` (readiness, flips to `503` while draining).
TLS via rustls; per-tool **ReBAC** capability gating; append-only
`.audit.jsonl`.

## Replication & failover ⚠️ primitives shipped, failover is manual

nDB replicates PostgreSQL-style — base backup + WAL log-shipping:

- **Bootstrap a follower:** `Engine::backup_to(dest)` on the leader
  produces a consistent base backup; open a follower engine on it.
- **Stream:** the follower tracks a `FollowerCursor` (a WAL byte offset)
  and pulls records past its watermark from the leader's `POST /replicate`
  route (`Engine::serve_replication`); it applies them with
  `Engine::ingest_replicated` / `replication::apply_batch`. Because the
  follower only appends bytes the leader already made durable — never
  re-stamping tx ids — its MVCC view is byte-for-byte identical.

### Manual failover runbook

There is **no automatic leader election** yet. To promote a follower:

1. **Stop writes to the old leader** (take it out of the write path / fence
   it at the load balancer) to avoid split-brain.
2. **Drain replication lag:** let the follower pull until its cursor
   reaches the leader's current WAL offset (compare via `/status` +
   replication response watermarks).
3. **Promote:** point the application's writer at the follower; it already
   has the full, consistent state. Begin accepting writes there.
4. **Re-seed the old leader** as a new follower from a fresh `backup_to`
   once it returns.

Until a coordinator automates steps 1–3, run failover as a supervised
operational procedure (or front it with an external consensus layer).

## Operational checklist

- [ ] `NDB_ENC_KEY` set (if encryption required) and backed up in a secret
      manager.
- [ ] Bearer token / ReBAC principals configured; TLS terminated.
- [ ] `/metrics` scraped; alert on `ndb_connections_rejected_total`,
      rising `sstable_count`, and replication lag.
- [ ] Periodic `backup_to` to off-host storage.
- [ ] Liveness probe → `/health`, readiness probe → `/ready`.

## Honest roadmap (not yet done)

| Item | Effort | Notes |
|---|---|---|
| Automatic failover / leader election (Raft) | Large | Today: manual runbook above. The bytes-identical replication makes a coordinator tractable, but consensus is its own project. |
| Horizontal sharding | Large | Single-writer per database; scale-out needs a partitioning layer above the engine. |
| Coverage-guided fuzzing in CI | Medium | `fuzz/` targets shipped (`cargo +nightly fuzz run …`); wiring a time-boxed run into CI is the remaining step. |
| Structured tracing (request spans, slow-query log) | Medium | Counters exist in `/metrics`; a `tracing` integration would add per-request spans + latency histograms. |
| Additional language SDKs (JS/TS, Go) | Medium | Rust + Python clients exist; the wire protocol is small and documented. |
| GPUDirect Storage (disk → GPU) | Large | Deferred; see `docs/roadmap-agent-gpu.md`. |
