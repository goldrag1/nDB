# Task: Streaming query executor — close the two_pattern_join 50-80× gap

## Context

The v3 controlled-race and stress-race results both expose the same
architectural ceiling: nDB's query executor materialises every
intermediate result row into a `Vec<Bindings>` before passing to the
next pattern. For single-pattern lookups and adjacency walks (bounded
fanout), this is fine. For two-pattern joins where the seed produces
N rows × M per-row downstream lookups, it produces an O(N×M)
intermediate that PG's HashJoin amortises into O(N + M).

Live numbers (bench_race, 100k records):

| workload | nDB controlled p50 | PG p50 | gap |
|---|---:|---:|---:|
| two_pattern_join | 46 ms | 0.6 ms | PG 80× |
| recursive_contains_depth3 | 37 μs | 968 μs | nDB 26× |

The recursion case wins because the bounded depth caps fanout; the
flat join loses because PG has hash-build-probe and nDB doesn't.

This task is the v2 priority called out in `docs/intro.md` and
`docs/hn-comment-opener.md`: rewrite the executor as a streaming
iterator pipeline + add a streaming hash-join. Estimated impact: the
50-80× loss shrinks to ≤2× (and may flip to a win at small N).

Reference issue: discussion-storage.html notes this is on the roadmap.

## Scope

- Project: `/home/long/long/nDB-ndimemsion-database`
- Branch: create new `feat/streaming-executor`
- Related files:
  - `crates/ndb-engine/src/query/mod.rs` — `pub fn execute()`, the
    `execute_pattern`, `execute_entity_pattern`,
    `execute_hyperedge_pattern`, `execute_recursive_hyperedge`
    helpers. ~1100 LOC. Heart of the rewrite.
  - `crates/ndb-engine/src/query/plan.rs` — planner stays as-is;
    streaming executor uses its order. Will need to expose more
    cardinality info if cost model gains weight.
  - `crates/ndb-engine/src/wire_query.rs` — `QueryRequest`,
    `QueryResponse`, `Pattern`, `Bindings` types. Bindings stays
    materialised at the *row* level; the *table* of bindings becomes
    an iterator.
  - `crates/ndb-server/tests/http_round_trip.rs` — integration test
    for `/query`. Must pass unchanged.
  - `docs/superpowers/specs/2026-05-27-query-language.md` — semantics
    are unchanged; this task is implementation only. If a semantics
    case surfaces during the rewrite (e.g., LIMIT pushdown
    ordering with the planner's reorder), update the spec.
  - `crates/ndb-engine/examples/realworld_bench.rs` — measure after.
  - `crates/ndb-engine/examples/bench_race.rs` — measure after.

## Requirements

### 1. Bindings table → iterator

Today: `let mut rows: Vec<Bindings> = vec![Bindings::new()];` walks
through every planned pattern, each pass calling
`rows = execute_pattern(engine, snapshot, pattern, rows)`.

After: replace with a pipeline where each pattern is an
`Iterator<Item = Result<Bindings, QueryError>>`. The pattern
constructors take an *upstream iterator* + their pattern and yield a
*downstream iterator*. The terminal stage either materialises (for
sort, aggregate) or projects/limits and streams to the response
writer.

Key types:

```rust
type BindingStream<'a> = Box<dyn Iterator<Item = Result<Bindings, QueryError>> + 'a>;

fn execute_pattern_stream<'a>(
    engine: &'a Engine, snapshot: TxId, pattern: &'a Pattern,
    upstream: BindingStream<'a>,
) -> BindingStream<'a>;
```

The Engine borrow is `&Engine` (immutable). The current
`execute_entity_pattern` takes `&mut Engine` because `entity_at` calls
`engine.snapshot_read` which takes `&mut self`. This task does NOT
fix the &mut-Engine issue (that's #3 RwLock task) but must keep the
pipeline coherent — either thread mutability through (acceptable for
now), or wrap reads in a small immediate-evaluation interior
mutability shim (cleaner). Pick one and stick with it.

### 2. Streaming hash-join for shared variables

When pattern N+1 shares a variable `?v` with the upstream bound set:

- If the upstream is bounded (`upstream.size_hint() ≈ small`), buffer
  the upstream into a `HashMap<Value, Vec<Bindings>>` keyed on `?v`,
  then stream pattern N+1's candidates probing the map. Classic
  hash-join build/probe.
- If the upstream is unbounded (recursive pattern, large type
  cluster), use a nested-loop with index lookup — pattern N+1 takes
  each upstream binding and uses the existing index for that pattern
  type.

The planner already orders patterns smallest-seed-first; this just
makes the join algorithm match the planner's intent. Decision rule:
build side = smaller of (upstream sketch, downstream type cluster).

### 3. LIMIT pushdown

When `limit` is set AND `order_by` is empty, the iterator can
short-circuit once N rows have flowed through. Use a `.take(n)`
adapter at the right stage:

- If no order_by: `.take(n)` at the end of the pipeline.
- If order_by is present: must materialise + sort first, then take.
  This is the existing behaviour; keep it.

For the join case: a `.take(n)` past the join automatically backs off
the probe side once N hits land.

### 4. Aggregations: materialise only the group key set

Today: `aggregate_rows` collects every row, groups, computes. For
COUNT this is wasteful — counter increments are commutative.

Refactor `aggregate_rows` to a streaming `fold` that:
- For each incoming row, computes the group key.
- Updates the running aggregator state per group (count++, sum +=,
  min = min(min, x), etc.).
- After the iterator drains, emits one row per group.

Memory: O(distinct groups) instead of O(input rows).

### 5. Where clause: filter combinator

`req.filter` becomes a `.filter(|r| eval_filter(expr, r))` adapter on
the iterator. No semantics change.

### 6. Recursive patterns: still buffered

Recursive walks (`contains+`, `contains*`) inherently need bounded
BFS with a visited set. Keep them materialising internally (no
change). They yield a `BindingStream` at the top of the iterator.

### 7. Cost model — defer to next session

Don't add a real cost-based optimiser here. The greedy planner's
order is correct *as input* to the streaming executor; the speedup
comes from the new join strategy, not from re-planning. Note in the
plan.rs preamble that "cost-based planning is v2.x work".

## Acceptance Criteria

Each must be checkable by the agent. Be specific.

- [ ] All 297+ existing `cargo test -p ndb-engine` tests pass
  unchanged.
- [ ] `cargo test --workspace` exits 0.
- [ ] New test
  `query::tests::two_pattern_join_uses_streaming_hash_join` —
  builds an engine with 100 entities + 100 hyperedges, executes
  `match X(a: "z") as ?c Y(buyer: ?c) return ?c` and asserts:
  - The result count matches an oracle implementation that walks
    the records imperatively.
  - The intermediate-bindings count (instrumented via a debug
    counter on the streaming pipeline) is ≤ 1.2× the result count
    + the seed count. (No O(N×M) materialisation.)
- [ ] New test `query::tests::limit_pushdown_short_circuits_join` —
  builds a 1000-customer × 10-sale-per-customer dataset, runs
  `match X() as ?c Y(buyer: ?c) return ?c limit 5`, asserts the
  pipeline's probe-side counter is ≤ 100 (not 10,000).
- [ ] `cargo run --release --example realworld_bench` shows
  `two_pattern_join.p50_us` ≤ 1.5 ms (was ~46 ms) at the existing
  workload sizing. Commit the new numbers to
  `docs/benchmarks/realworld-2026-05-XX.md`.
- [ ] Same bench: no regression on any other workload
  (`point_lookup`, `property_lookup`, `single_pattern_query`,
  `recursive_contains_depth3`, `count_aggregate`,
  `commits_per_sec`) — all within ±20% of pre-rewrite numbers.
- [ ] `crates/ndb-engine/examples/bench_race.rs` rebuilds + serves
  the new executor; live race at `/bench.html` shows
  `two_pattern_join` controlled p50 ≤ 1.5 ms.
- [ ] Commit on `feat/streaming-executor`, push, open PR with
  before/after bench table in the description.

## Constraints

- Do NOT change query language semantics. Same wire-AST in, same
  rows out (modulo no-order-by-then-limit may now return a
  different N-tuple — that's fine and consistent with the spec).
- Do NOT change `Bindings` storage (it stays `HashMap<String, Value>`).
- Do NOT add new dependencies. Stdlib + serde + uuid only, like the
  rest of the crate.
- Do NOT touch the planner's cost estimation logic. It stays
  greedy-smallest-seed.
- Do NOT split this across multiple PRs — it's one atomic rewrite of
  the executor pipeline. Even if it takes 5 sessions, ship as one PR.

## Verification Commands

cd /home/long/long/nDB-ndimemsion-database && cargo build -p ndb-engine
cd /home/long/long/nDB-ndimemsion-database && cargo test -p ndb-engine query 2>&1 | tail -8 | grep -q 'test result: ok'
cd /home/long/long/nDB-ndimemsion-database && cargo test --workspace 2>&1 | grep -q 'FAILED' && exit 1 || exit 0
test -f docs/benchmarks/realworld-*.md
grep -q 'streaming' crates/ndb-engine/src/query/mod.rs

## Live Testing Procedure

After acceptance criteria pass:

1. cd /home/long/long/nDB-ndimemsion-database
2. cargo build --release --example realworld_bench -p ndb-engine
3. ./target/release/examples/realworld_bench > /tmp/realworld-after.json 2> /tmp/realworld-after.log
4. Compare two_pattern_join, point_lookup, count_aggregate p50 between
   /tmp/realworld-after.json and docs/benchmarks/realworld-2026-05-28-ndb.json
5. cargo build --release --example bench_race -p ndb-engine
6. bash ~/.local/bin/ndb-services-launcher.sh restart
7. Open http://127.0.0.1:9880/bench.html#controlled, race two_pattern_join,
   verify p50 ≤ 1.5 ms in the result lane

## Agent Persona

You are a senior Rust engineer working on a database engine. Read
`crates/ndb-engine/src/query/mod.rs` end-to-end before writing any
code. The current materialise-everywhere implementation is
deliberate (it's correct + simple) and must continue to pass every
existing test. The streaming rewrite is purely a performance
optimisation; correctness comes from preserving the planner +
unification semantics that already work.

Read these supporting docs first:
- `docs/superpowers/specs/2026-05-27-query-language.md` — semantics
- `docs/superpowers/specs/2026-05-27-v2-working-spec.md` — planner
- `~/.claude/rules/programming.md` — bias-toward-simplicity rule

## Model

opus

## Time Budget

- Session minutes: 25 (this is a focused-engineering task; longer
  contexts let you keep the type graph in your head)
- Max sessions: 8
- Total hours: 4

Stop early if every acceptance criterion passes after 3 sessions —
no padding.
