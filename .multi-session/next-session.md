# Next session prompt — finish #1 streaming executor + ship #3 RwLock engine, then race

Paste this verbatim as the user prompt for the next Claude Code session
on this repo. Single Opus 4.7 session, no multi-session orchestration.

---

You're picking up the nDB project at `/home/long/long/nDB-ndimemsion-database`
(branch: `main`, tip: see `git log -1`). Read `MEMORY.md` + skim
`docs/superpowers/specs/2026-05-27-query-language.md` first.

Current state (just shipped in the previous session):
- v3 perf trio merged:
  - `count()` pushdown via new `EntityTypeIndex` — 30-210× loss → 700× win
  - Block-index sidecar wired into `snapshot_read` (`find_all` added) — cold p50 ~27 ms → 22 μs
  - Type-bucket-hoist in `candidate_hyperedges` — two_pattern_join 46 ms → 1.6 ms (29×)
- Live race at `/bench.html` reflects new numbers (controlled tab reference table refreshed)
- Bench doc updated: `docs/benchmarks/realworld-2026-05-28.md` + `*-{ndb,pg}.json`
- Tests: 297 ndb-engine + 73 ndb-query + 32 ndb-server + 10 ndb-client-rust, workspace green

Two tasks for THIS session (single Opus 4.7, no multi-session):

═══════════════════════════════════════════════════════════════════════
TASK 1 — Finish #1: streaming-executor rewrite
═══════════════════════════════════════════════════════════════════════

Plan doc with full design: `.multi-session/streaming-executor.md`.
Treat that file as the source of truth for shape + acceptance.

Headline: convert `query::execute()` from
`let mut rows: Vec<Bindings> = vec![Bindings::new()];`
+ per-pattern materialise loops, to an `Iterator<Item = Result<Bindings, _>>`
pipeline. Add a streaming hash-join when patterns share a variable.
LIMIT pushes through via `.take(n)`. Aggregation becomes a streaming
fold.

Why this matters even after v3-perf: the type-bucket hoist closed the
worst symptom on two_pattern_join (46 ms → 1.6 ms) but the
materialise-bindings architecture is still there. As N grows the gap
widens again. The bench currently shows nDB 1.6 ms vs PG 0.5 ms = PG
~3×; streaming should land it at ≤1× (nDB wins) for N=100k.

Acceptance:
- Every existing 297+ ndb-engine test still passes
- New test `query::tests::two_pattern_join_uses_streaming_hash_join` — see
  spec for the instrumentation counter; intermediate-bindings count
  ≤ 1.2× (result_count + seed_count)
- New test `query::tests::limit_pushdown_short_circuits_join` — probe-side
  counter ≤ 100 (not 10,000) on a 1000×10 dataset with LIMIT 5
- `realworld_bench` `two_pattern_join.p50_us` ≤ 1 ms (was 1.6 ms before)
- No regression > ±20% on any other workload

Constraints:
- Same query-language semantics (wire AST in, same rows out modulo no-order-by + limit)
- Bindings storage stays `HashMap<String, Value>`
- No new dependencies
- Engine stays `&mut self` on reads (don't conflate with Task 2)

═══════════════════════════════════════════════════════════════════════
TASK 2 — Ship #3: RwLock<Engine> for concurrent reads
═══════════════════════════════════════════════════════════════════════

Goal: snapshot_read, property_lookup, hyperedges_by_type,
entities_by_type, all read-path Engine methods take `&self` instead of
`&mut self`. Bench backends switch their `Mutex<Engine>` to
`RwLock<Engine>` so the 64-client stress race actually parallelises.

Required changes (scan before editing):
- `crates/ndb-engine/src/engine.rs` — read methods to `&self`:
  - `snapshot_read`, `snapshot_iter`, `snapshot_iter_streaming`
  - `lookup_by_external_key`, `property_lookup`, `property_range`
  - `entities_by_type`, `entity_type_count`, `hyperedges_by_type`,
    `hyperedge_type_count`, `adjacency_degree`, `adjacency_overview`,
    `hyperedges_for_entity`
  - `vector_search`
  - Any helper called by the above
- `crates/ndb-engine/src/memtable.rs` — `versions()` already takes `&self`;
  verify nothing internal needs interior mutability
- `crates/ndb-engine/src/sstable.rs` — `SSTableReader::find_all`/`find`/`iter`
  take `&self` already; verify
- Index module:
  - `index/lookup_key.rs` — read methods to `&self`
  - `index/property_btree.rs` — `find`, `range` to `&self`
  - `index/adjacency.rs` — `neighbors`, `neighbors_vec`, `degree` to `&self`
  - `index/type_cluster.rs` — `by_type`, `by_type_vec`, `count` to `&self`
  - `index/entity_type_cluster.rs` — same
  - `index/vector.rs` — `search` to `&self`
- `crates/ndb-engine/src/query/mod.rs` — every `execute_*` function's
  `engine: &mut Engine` becomes `engine: &Engine` for read patterns;
  write patterns (create/delete/set/merge) keep `&mut`
- `crates/ndb-engine/src/query/plan.rs` — `plan()`, `estimate_cardinality`,
  `explain()` all to `&Engine` (already mostly there)
- `crates/ndb-server/src/lib.rs` — handlers acquire read lock for
  read routes; write lock for /commit, /flush, /compact, /query write
  clauses
- `crates/ndb-engine/examples/bench_race.rs` — `state.engine: RwLock<Engine>`;
  stress workers take `.read()`, the (eventually) commits_per_sec would
  take `.write()` (still gated out for now)

Acceptance:
- Every test in the workspace still passes
- New stress test `engine::tests::concurrent_point_lookups_scale` — spawn
  16 threads, each doing 10k point_lookups against the same Arc<RwLock<Engine>>,
  assert wall time < (1/8 × single-thread baseline) — i.e., RwLock actually
  parallelises
- `bench_race` `/stress` at concurrency=16 on point_lookup shows ≥ 4×
  the throughput of concurrency=1 (was ≤1× before — Mutex queue)
- No regression on any single-threaded benchmark

Constraints:
- This is a TYPE SURFACE change, not a behaviour change. Every test
  should pass without semantic edits.
- Don't introduce parking_lot or other deps — use std::sync::RwLock.
- Writes still serialize (single-writer model preserved).
- If a method genuinely needs `&mut self` (writes), keep it that way
  and route through `.write()` at the call site.

═══════════════════════════════════════════════════════════════════════
TASK 3 — Re-race + push
═══════════════════════════════════════════════════════════════════════

After both tasks land:

1. Rebuild release binaries:
       cargo build --release --example bench_race -p ndb-engine
       cargo build --release --example realworld_bench -p ndb-engine
2. Restart services:
       bash ~/.local/bin/ndb-services-launcher.sh restart
3. Run realworld bench fresh:
       ./target/release/examples/realworld_bench > /tmp/ndb-realworld-new.json 2> /tmp/ndb-realworld-new.log
4. Run PG bench fresh:
       python3 tools/bench/realworld_pg.py > /tmp/pg-realworld-new.json
5. Replace committed reference numbers:
       cp /tmp/ndb-realworld-new.json docs/benchmarks/realworld-2026-05-28-ndb.json
       cp /tmp/pg-realworld-new.json  docs/benchmarks/realworld-2026-05-28-pg.json
       python3 tools/bench/render_realworld.py --ndb docs/benchmarks/realworld-2026-05-28-ndb.json --pg docs/benchmarks/realworld-2026-05-28-pg.json --title "Real-world micro-benchmark — nDB v1.3 vs PostgreSQL (post v3-final)" > docs/benchmarks/realworld-2026-05-28.md
6. Refresh the inline reference table in `docs/knowledge-site/bench.html`
   (search for `Reference run · 2026-05-28` and update each row from
   the new JSON; keep the "post v3-final" title)
7. Update `tools/bench/render_realworld.py`'s notes section so the
   numbers in the "What changed since the prior reference run" table
   match the new measured values. Don't regenerate from a stale prior.
8. Live verify via Playwright on `/bench.html` — race count_aggregate
   at conc=64 + two_pattern_join at conc=16 + point_lookup at conc=64.
   Screenshot the stress page and send it.
9. One commit per task (3 commits total), then push.

Quality gate before declaring done:
- workspace cargo test green
- live `/bench.html` shows the new numbers under "Reference run"
- the community-aggregates panel still works (refreshAggregates +
  /api/race/log shouldn't have broken)
- send a 3-bullet summary back to the user with the new numbers

Budget: ~3-4 hours of focused work. Stop early if both tasks land
under budget.

═══════════════════════════════════════════════════════════════════════
HELP / DEFERRED
═══════════════════════════════════════════════════════════════════════

- The streaming executor task spec in `.multi-session/streaming-executor.md`
  has more detail on each stage (LIMIT pushdown shape, aggregation fold,
  recursion handling). Read it.
- If RwLock surfaces unforeseen lifetime issues with the index types
  (BTreeSet iterators borrowing inside RwLockReadGuard), the escape
  hatch is `parking_lot::RwLock` (already common in the Rust DB
  ecosystem). Don't pull it unless absolutely necessary; std::sync
  should work for the access patterns here.
- Don't touch the live-race UI semantics unless the wire shape needs
  to change (it shouldn't).
- The whitepaper + intro.md + hn-comment-opener.md still reference the
  old numbers. Updating them is its own commit if you want — not
  required for this session's scope.
