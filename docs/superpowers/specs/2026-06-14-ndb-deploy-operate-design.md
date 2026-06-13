# nDB Deploy & Operate — Design (Stage 4)

**Date:** 2026-06-14
**Sequence:** Sub-project 2 of the general-purpose DBMS product roadmap.
Follows **Adoptable Core** (Stage 1–3, merged). Precedes **Scale** (Stage 5:
sharding, 10GB perf).

## Goal

Make nDB **easy to deploy and operate across multiple servers** — the
explicit ask. After this stage: stand up a leader + auto-following replica
with one command, deploy to Kubernetes with health-gated rollouts, and see
per-route latency + slow queries in operation.

## The enabling gap

The engine already replicates PostgreSQL-style (base backup + WAL
log-shipping): `Engine::backup_to`, `serve_replication`, `ingest_replicated`,
`FollowerCursor`. The **leader** side is wired (`POST /replicate`). But there
is **no follower mode** in the server binary and no client pull method — a
replica can't follow a leader without external glue. Multi-server is therefore
not yet "easy." This stage closes that.

## Components

### 0. Base-backup-over-HTTP bootstrap (prerequisite — discovered 2026-06-14)

**A correct follower must bootstrap from the leader's base backup before
streaming.** The pull watermark is `(wal_seq, byte_offset)`; those only line up
with the leader if the follower's WAL *is* a byte-for-byte continuation of the
leader's — i.e. it started from `backup_to`. A follower that starts empty and
streams from the leader's current segment **silently diverges** if the leader
rotated/pruned its WAL before the follower joined (`poll_once` returns
`Rotated` for the pruned case, but fresh-empty-follower offset 0 against an
already-rotated leader has no safe meaning).

`Engine::backup_to(dest)` exists but is **local-only — no HTTP route**. So this
stage must add:
- Leader: `GET /v1/backup` — stream a consistent base backup (e.g. tar of the
  DB dir produced by `backup_to` to a temp), plus the `(wal_seq, offset)`
  watermark it corresponds to.
- Follower bootstrap: on first start with an empty DB, `GET /v1/backup`,
  restore it, initialise the `FollowerCursor` at the backup's watermark, then
  enter the pull loop.

This is the real keystone; component 1 builds on it.

### 1. Server follower mode (the enabler) — build on §0
- New server flags: `--replicate-from <leader-url>` and
  `--replicate-interval <secs>` (default 2s). Optional `--replicate-token` for
  the leader's bearer auth.
- On start with `--replicate-from`, the server spawns a background **pull
  loop**: POST the leader's `/v1/replicate` with the local `FollowerCursor`
  watermark, apply returned records via `ingest_replicated`, advance the
  cursor, sleep, repeat. Backs off on error; never re-stamps tx ids (the
  engine guarantees byte-identical MVCC).
- A follower serves **reads** normally (its `/v1/...` read routes) and rejects
  **writes** with `503 read_only_follower` (writes go to the leader).
- Outbound HTTP: reuse `ndb-client-rust` (already the typed client) for the
  pull call, or a minimal raw POST — whichever keeps the dependency graph clean.
- **Test:** spawn a leader + a follower in-process; commit to the leader; assert
  the record appears on the follower within a few intervals; assert a write to
  the follower returns `503 read_only_follower`.

### 2. Deploy artifacts
- **docker-compose.yml**: a leader + one follower (follower started with
  `--replicate-from http://leader:8742`), persistent volumes, health checks on
  `/v1/health`. `docker compose up` → a live replicating pair.
- **Helm chart** (`deploy/helm/ndb/`): a `StatefulSet` (stable network id +
  per-pod volume), a `Service`, liveness probe → `/v1/health`, readiness probe
  → `/v1/ready`, configurable replica count where replica pods run follower
  mode pointed at the `-0` leader. `values.yaml` covers image, storage size,
  token/TLS secrets, replicate interval.
- **README** in `deploy/` documenting compose + Helm + the single-binary
  systemd pattern.

### 3. Observability
- Extend the existing `/metrics` (already has `ndb_requests_total{route}` +
  duration sum/count) with **per-route latency buckets** (a Prometheus
  histogram) so p50/p95/p99 are derivable.
- **Slow-query log**: requests exceeding a threshold
  (`--slow-query-ms`, default off) emit a structured line
  (`{route, method, duration_ms, status, principal}`) to the audit/stderr sink.
- Keep it dependency-light (no heavyweight `tracing` stack unless it pays for
  itself); the histogram + slow-log cover the operator's first questions.

## Automatic failover (Raft) — explicitly scoped OUT of this stage

Leader election / automatic failover is a **Large**, standalone effort that
changes the write path and deserves its own sub-project — not a sub-task here.
What this stage delivers instead:
- Follower mode + health/readiness probes make a **k8s-native or
  external-coordinator** failover tractable (promote a replica, repoint the
  Service/writer).
- The manual failover **runbook** already in PRODUCTION.md stays the
  documented procedure.
- A future "HA / Consensus" sub-project will add automatic election (e.g.
  `openraft` over the engine, or an external coordinator driving the existing
  promote primitives).

## Non-goals (this stage)
Automatic leader election/Raft, horizontal sharding, 10GB-scale perf,
GPUDirect. (Raft → its own sub-project; sharding + perf → Stage 5.)

## Success criteria
1. `docker compose up` → a leader + follower; a write to the leader is readable
   on the follower within a few seconds; a write to the follower is `503`.
2. `helm install` → pods become Ready via `/v1/ready`; replica pods follow `-0`.
3. `/metrics` exposes a per-route latency histogram; enabling `--slow-query-ms`
   logs slow requests in a structured line.

## Build order
1 (follower mode + test) → 2 (compose, then Helm) → 3 (observability). Each
verified and committed independently.
