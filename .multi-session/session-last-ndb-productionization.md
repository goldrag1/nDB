## Session 2026-06-14 — nDB productionization: Stages 1–5 (Adoptable Core → Deploy → Scale)

### Đã làm (all merged to main, every increment tested + its own commit)
- **Git hygiene first:** cleaned remote (deleted ~14 merged/stale branches → remote now just `main` + `develop`); rescued 9 stranded local-only `feat/nstack` studio commits (edges-on-edges, storage panel, graph explore, time-travel, bulk edit, command palette, integrity, README) by squash-rebasing onto current main (engine had already graduated edges-on-edges + hyperedge properties, so the studio layer wired straight on).
- **Stage 1–3 Adoptable Core** (general-purpose DBMS product target): `/v1` wire-protocol prefix (bare routes → deprecated aliases) + `docs/PROTOCOL.md`; "your data always opens" format-compat guarantee + `docs/COMPATIBILITY.md` (4-surface semver, backed by the existing v2-byte-stream decode test); `@ndb/client` zero-dep typed TS SDK (`clients/ts`, 5 integration tests vs a real server); `@ndb/mcp` npx-runnable launcher (`clients/mcp-npm`); Dockerfile + `release.yml` + CI gates; two-path QUICKSTART.
- **Stage 4 Deploy & Operate:** follower-mode replication (the keystone) — `GET /v1/backup` base-backup-over-HTTP + `bootstrap_follower_if_needed` + `run_follower_loop` + `--replicate-from` CLI (retrying bootstrap) + read-only replica; `docker-compose.yml` (leader+follower); Helm chart (`deploy/helm/ndb`, StatefulSet pod-0 leader); observability (`/metrics` latency histogram + `--slow-query-ms` slow-query log). `crates/ndb-engine/src/backup_archive.rs` (dep-free flat-file archive) + `Engine::replication_watermark()`.
- **Stage 5 Scale:** perf finding (10GB explorer gap already closed by tile cache — `docs/explorer/PERFORMANCE.md`), so Stage 5 = sharding. Full sharding design (`docs/superpowers/specs/2026-06-14-ndb-scale-sharding-design.md`) + new `crates/ndb-router` coordinator: hash routing (D1), point read (hash-first + scatter-on-miss), commit routing (entity→owner, hyperedge→anchor D2, dict broadcast), `iter` scatter, vector kNN top-k merge, cross-shard `traverse` (hop-by-hop scatter+union), `docker-compose.sharded.yml`. 6 router tests.

### Quyết định quan trọng
- Product target = **general-purpose DBMS** (user chose over dogfood/platform). SDK = thin typed HTTP client (not ORM/codegen). Format promise = "always opens" (free — code already retains old decoders). Protocol = `/v1` URL prefix. Agent packaging in-scope.
- Sharding D1 = **hash(entity_id)**; D2 = **anchor shard** (hyperedge lives on shard of its first role-filler). D3–D6 = scatter-gather reads, top-k kNN merge, anchor-only commit (no distributed txn → 502 partial_commit), stateless `ndb-router` coordinator (SDK/MCP unchanged), fixed shard count.
- Raft/auto-failover + online resharding = explicitly scoped to their OWN sub-projects (not rushed).

### Learnings (cho session sau) — see memory `ndb-productionization` + promotions below
- nDB error envelope is `{"error":"<code>","detail":"<msg>"}` (NOT nested). `/iter` is JSONL. commit `kind` is serde snake_case → **`hyper_edge`** not `hyperedge`. `/commit` → `{tx_id}`.
- Sharding correctness: anchor placement ⇒ edge point-read by id needs scatter-on-miss; cross-shard neighbors needs router-driven hop-by-hop scatter+union (a single shard's `hyperedges_for_entity` misses non-anchor-member edges).
- A pull-follower MUST bootstrap from a base backup or it silently diverges (WAL watermark alignment).
- `cargo test --workspace` intermittently fails one ndb-server arrow test under cross-binary parallel load (many test binaries spawning HTTP servers) — passes isolated; not a regression.

### Trạng thái hiện tại
Core product roadmap Stages 1–5 shipped + merged. `main` clean. ndb-router routes all ops (read/commit/iter/kNN/traverse) across real multi-shard clusters, tested. SDK + MCP + Docker/Helm/compose all in place (infra is CI-verified, not locally — no docker/k8s in the session env).

### Update — v2.4.0 RELEASED + verified (2026-06-14, later in session)
Cut **v2.4.0** (bumped Cargo workspace 1.3.0→2.4.0 to rejoin the git-tag line). `release.yml` ran: binaries attached to the GitHub release ✓, multi-arch Docker image building (ghcr.io/goldrag1/ndb), npm published. **Published packages smoke-tested working** from npm: `@n-dimension-database-ndb/client@2.4.0` (real commit→read) + `@n-dimension-database-ndb/mcp@2.4.0` (npx → per-platform binary → MCP tools). npm org = **`n-dimension-database-ndb`** (short `ndb` was taken). README updated to lead with install paths.
Release bugs found+fixed (see new rule file `release-publishing.md`): (1) `release.yml` cp'd binary by crate name `ndb-cli` but the bin is `ndb`; (2) JS package.jsons hardcoded 0.1.0 while per-platform packages stamped from tag (2.4.0) → broken optionalDeps → fixed by stamping tag version at publish; (3) GHA secrets read at job start (set NPM_TOKEN mid-run only helped the later job); (4) npm `latest` = last-publish-wins → repointed to 2.4.0.

### Next Session Task
Immediate: confirm the Docker image landed (`docker run -p 8742:8742 ghcr.io/goldrag1/ndb` + curl /v1/health — couldn't verify in-session, no docker). Then pick ONE follow-up (none blocking):
1. **Helm sharded topology** — router Deployment + N shard StatefulSets in `deploy/helm/` (tractable YAML; needs a real cluster to verify). Smallest.
2. **Online resharding** — LARGE: its own brainstorm+design (consistent hashing / range moves / live migration) before code. Don't rush.
3. **Raft auto-failover** — LARGE: its own sub-project (per-shard leader election); follower-mode + health probes already make a coordinator tractable.
Minor cleanup: the router's 501 fallback message in `route()` omits `/v1/traverse` from its list (cosmetic, error path only).

### Remaining (chưa xong)
- [ ] Verify the v2.4.0 Docker image published (`docker run ghcr.io/goldrag1/ndb` + curl /v1/health) — needs: a docker-capable machine
- [ ] Rotate exposed npm tokens (npm_r8sgh… dead, npm_XOiS… in transcript) → new automation token → `gh secret set NPM_TOKEN`; optionally `npm deprecate @…/client@0.1.0` + `@…/mcp@0.1.0` (stray pre-fix versions)
- [ ] Helm sharded chart (router Deployment + shard StatefulSets) — needs: write YAML, verify on a real k8s cluster
- [ ] Online resharding — needs: a dedicated design session (architecture sign-off) before any code
- [ ] Raft auto-failover — needs: its own sub-project
