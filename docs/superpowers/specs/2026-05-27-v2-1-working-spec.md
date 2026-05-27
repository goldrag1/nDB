# nDB v2.1 — Working Spec

> **Status:** Drafted 2026-05-27, opens immediately after v2.0.0 shipped.
> Locks the v2.1 release identity, scope, sequencing, and success
> criteria. v2.0 successor; tighter scope, shorter sprint.

## 1. Identity

**v2.1 is the "finish v2.0 + demo the N-dim story" release.** Three
goals:

1. Close v2.0's two follow-ons: `Engine::reencrypt` (deferred from
   §6 of the v2.0 spec) and server auth dispatch through
   `Engine::has_capability` (capability revocation effective without
   restart).
2. Broaden the `ndb-slicer` analytics surface — percentiles, `HAVING`,
   time-bucket binning, multi-column sort. `ndb-slicer` is the one
   user-facing API where "correct but minimal" still describes the
   v2.0 state.
3. Build the **N-dimensional renderer story**. v2.0 ships tabular
   output (text / TSV / CSV) which is fundamentally 2D and doesn't
   showcase why a hyperedge-native database earns the name "nDB".
   v2.1 adds three self-contained Rust → HTML+SVG renderers — pivot
   table, parallel coordinates, hypergraph diagram — so a single
   `.html` file demonstrates 4-20 dimensional data without a
   frontend project.

This is **NOT** a platform release. No new transports, no distributed
mode, no §12 grammar additions, no separate frontend codebase. The
on-disk format is unchanged; v2.0 databases open in v2.1
byte-for-byte and the reverse holds modulo the new analytics being
unused.

Critical-path constraint: v2.1 must ship **before** v3 work begins, so
the v3 working spec can focus on platform-shape decisions (distributed
mode, write-via-query, gRPC, JS/Go clients, interactive React/D3
explorer) without inheriting v2-era caveats.

## 2. Scope — locked deliverables

Three sprints, each tightly scoped. Items inside one sprint are
independent and parallelize.

### Sprint 1 — Encryption migration + auth dispatch (1-1.5 weeks)

These two close the v2.0 spec gaps. They share no code but together
they're "what 'encryption support' really means" + "what 'ReBAC
hyperedges' really means" — the rough edges that v2.0 documented as
follow-ups.

#### 2.1 `Engine::reencrypt(new_cipher: Option<Cipher>)`

Goal: a single operator-facing API that migrates a database between
encryption states. Covers the three transitions v2.0 deferred:

  - **Plaintext → encrypted.** Engine has no `.encryption` marker; new
    cipher is `Some(_)`. Rewrite every SSTable through `EncryptedFile`,
    rewrite the active WAL segment, write the marker last.
  - **Encrypted → encrypted (new key).** Engine has a marker; new
    cipher is `Some(_)` with a different fingerprint. Decrypt + re-encrypt
    every SSTable + the WAL with the new key, then write a new marker.
  - **Encrypted → plaintext.** New cipher is `None`. Strip encryption
    from every SSTable + the WAL, then delete the marker.

Design constraints:

  - **Crash safety.** Standard write-temp-then-rename per file, plus a
    transient `.encryption.next` marker that captures the in-progress
    migration state (target fingerprint, set of SSTable seqs already
    rewritten). On crash mid-migration, `Engine::open` consults
    `.encryption.next` and either resumes or rolls back (file by file
    — each is its own atomic unit).
  - **Snapshot guard.** Refuse to migrate while `SharedEngine` has any
    active snapshots registered. Caller releases readers first.
  - **MANIFEST rewrite.** The MANIFEST entries don't carry encryption
    state per-file in v2.0 — encryption is per-database. Keep this:
    no MANIFEST changes; the marker is the single source of truth.

API:

```rust
impl Engine {
    /// Migrate this database between encryption states. Idempotent
    /// when the target state already matches.
    pub fn reencrypt(&mut self, new_cipher: Option<Cipher>) -> Result<MigrationStats, EngineError>;
}

pub struct MigrationStats {
    pub sstables_rewritten: usize,
    pub wal_segments_rewritten: usize,
    pub bytes_rewritten: u64,
}
```

Tests:

  - Plaintext → encrypted → plaintext round-trip preserves every record.
  - Crash mid-migration (kill the rewrite at a known SSTable boundary):
    next `Engine::open` either completes or rolls back; never lands in
    a mixed state.
  - Refuse while a `SharedEngine` snapshot is held.
  - `Engine::reencrypt` with the current state returns `Ok(zero-stats)`
    and doesn't touch disk.

Effort: 4-5 days. Crash safety is the entire cost; the rewrite loop
itself is mechanical.

#### 2.2 Server auth dispatch through `Engine::has_capability`

Goal: the hot path of every gated route reads the engine, not the
in-memory cache. Capability hyperedges committed via `/commit` become
effective on the next request without restart.

Design:

  - Server's `dispatch` calls `Engine::has_capability(principal_eid,
    action_str, target_str, now_us)` instead of `Principal::allows(cap)`.
  - The in-memory `Principals` cache shrinks to a **token → principal_eid
    + name** lookup. No `capabilities: BTreeSet<Capability>` field.
  - On startup the cache is built once via `Engine::principal_by_token`
    iteration; subsequent commits to principal/capability records
    don't invalidate the cache (the engine query is authoritative;
    the cache just resolves the token).
  - `Capability::as_action` + `from_action` stay (used by the
    bootstrap importer for the JSON shape); the runtime check uses
    `as_action(req_cap).to_string()` to compute the action key.

Wire surface unchanged.

Tests:

  - New capability committed via `/commit` is effective on the next
    request (no server restart).
  - Revoked capability (delete hyperedge via commit tombstone) takes
    effect on the next request.
  - Token rotation (new principal entity with new token, old one
    tombstoned) works end-to-end.

Effort: 3-4 days. Migration is mechanical; the test matrix is the cost.

### Sprint 2 — Slicer analytics polish (1.5-2 weeks)

The current slicer is correct but minimal. v2.1 closes the gaps that
make it useful for real analytics workflows — percentiles for SLO
queries, `HAVING` for "top groups", time-bucket binning for time-series
aggregation, multi-column sort for ordered grouping.

#### 2.3 Percentile aggregates

Goal: `Aggregate::Percentile(p)` for any `p ∈ (0.0, 1.0]`, plus
convenience constants for p50 / p95 / p99.

Design:

  - `Aggregate::Percentile { p: f64 }` variant. `p` is the fraction
    (0.50, 0.95, 0.99 — NOT 50/95/99).
  - Implementation: per-group `Vec<f64>` collected, sorted at fold time,
    linear interpolation between adjacent samples (R-7, the "default"
    method matching NumPy / pandas).
  - Numeric coercion: `Value::I64`, `Value::F64`, `Value::Timestamp`
    accepted; everything else contributes null and reduces the per-group
    count.
  - Memory cost: O(group_size × 8 bytes). Bigger than streaming
    aggregates; called out in the docstring. Streaming percentiles
    (t-digest, GK) are a v3 extension if real workloads need them.

Tests:

  - p50 on `[1, 2, 3, 4, 5]` = 3.0; p95 = 4.8; p99 = 4.96.
  - Empty group → null.
  - Cross-type input (one F64, one I64) coerces correctly.

Effort: 1.5 days.

#### 2.4 `HAVING`-style post-aggregate filter

Goal: drop rows from the aggregate output whose summary values don't
satisfy a predicate. Companion to `filter` which acts pre-aggregate.

Design:

  - `Pipeline::having<F>(self, f: F) -> Self where F: Fn(&[Value]) ->
    bool + Send + Sync + 'static`. Predicate receives the full row of
    the aggregate output (group key columns + aggregate columns) in the
    same order as `Pipeline::columns + Pipeline::aggregates`.
  - Applied AFTER aggregate fold, BEFORE sort + limit.

Tests:

  - `having(|row| row[1].as_i64() > Some(100))` drops every group
    whose sum is ≤ 100.
  - Combined with `filter` — pre + post both fire independently.

Effort: 0.5 days.

#### 2.5 Time-bucket binning

Goal: group rows by a fixed time interval on a `Value::Timestamp`
column, producing one row per bucket. The canonical use case is
"events per hour" / "revenue per day" reports.

Design:

  - New `ColumnSource::TimestampBucket { property: PropertyId, interval_us: i64, origin_us: i64 }`.
  - Reads the property as a `Value::Timestamp`, computes
    `((ts - origin_us) / interval_us) * interval_us + origin_us`,
    emits as `Value::Timestamp`.
  - `interval_us = 60_000_000` (1 minute), `3_600_000_000` (1 hour),
    `86_400_000_000` (1 day) — caller-supplied; no magic constants.
  - Works as a `group_by` key — the slicer's HashMap-based grouping
    handles it identically to any other column.

Tests:

  - Bucket events into hourly bins; assert one row per bin.
  - `origin_us` shift verified — non-midnight origins land at the
    right boundary.
  - Rows whose timestamp property is missing → null bucket → group
    skipped (matches existing null-key behaviour).

Effort: 1 day.

#### 2.6 Multi-column sort

Goal: `Pipeline::sort` accepts an ordered list of `(column_index,
ascending: bool)` keys. Ties on the first key break by the second key,
etc.

Design:

  - New `SortKey { column: usize, ascending: bool }` struct.
  - `Pipeline::sort(impl IntoIterator<Item = SortKey>)` replaces the
    private `sort_column + sort_asc` fields with a `Vec<SortKey>`.
  - `sort_asc(col)` / `sort_desc(col)` stay as one-key shortcuts
    (back-compat — every v2.0 caller still works).

Tests:

  - Sort by (region asc, revenue desc) — primary asc breaks ties via
    secondary desc.
  - Single-key compat: `sort_asc(0)` produces identical output to
    v2.0.

Effort: 1 day. Boilerplate; small.

### Sprint 3 — Renderer surface (0.5-1 week)

Three new output formats. Each is independent; mostly about correct
escaping + a clean public function signature.

#### 2.7 Markdown table renderer

Goal: GitHub-flavored Markdown table output for paste-into-issue /
paste-into-doc workflows.

Design:

  - `pub fn render_markdown(t: &Table) -> String`.
  - Header row + alignment row + body rows. Cells use `format_cell`
    (shared with text/TSV/CSV).
  - Escaping: backtick-wrap any cell containing `|`, `\n`, or leading
    `-` / `+` (Markdown bullet ambiguity). Newlines inside cells become
    `<br>` per GFM convention.

Tests:

  - Simple table round-trips with no escaping.
  - Cells containing `|` get backtick-wrapped.
  - Empty table emits header + alignment row only (no body), valid
    GFM.

Effort: 0.5 day.

#### 2.8 JSON-lines renderer

Goal: one JSON object per row, newline-delimited. Drop-in for
streaming pipes into `jq` / `duckdb` / Polars.

Design:

  - `pub fn render_jsonl(t: &Table) -> String`.
  - Each line is `{"<header_0>": <value_0>, "<header_1>": ...}`.
  - Value mapping mirrors `crate::wire::JsonValue` discriminator-free
    shape: I64/F64 → JSON number, String → JSON string, Bool → JSON
    bool, Timestamp → JSON number (microseconds), UUID → JSON string,
    Null → JSON null.
  - Streams via `String` for now; if memory becomes a concern, add
    `render_jsonl_writer<W: io::Write>(t, w)` later.

Tests:

  - One row per line in output.
  - Special characters in headers + values escape correctly.
  - Null values emit JSON `null`.

Effort: 0.5 day.

#### 2.9 HTML table renderer

Goal: minimal `<table>` for paste-into-email / paste-into-Confluence.
No CSS — plain semantic HTML.

Design:

  - `pub fn render_html(t: &Table) -> String`.
  - `<table>` + `<thead><tr><th>…` for headers + `<tbody><tr><td>…`
    for body. HTML-escape `<`, `>`, `&`, `"` in every cell + header.
  - No `<style>`, no classes — output stays small + Confluence-pasteable.

Tests:

  - HTML special chars escape correctly.
  - Output validates against a minimal HTML parser stub (or just
    string-shape assertions — we don't pull in an HTML validator dep).

Effort: 0.5 day.

### Sprint 4 — N-dimensional renderers (Rust, self-contained HTML+SVG) (2-2.5 weeks)

Three renderers that emit **one self-contained `.html` file** —
inline CSS, inline SVG, inline JS for tooltips, zero external assets.
Open in any browser, email to a teammate, embed in a doc, no build
step. This sprint is the "demo nDB's N-dimensionality" story; the
hypergraph diagram in particular is the deliverable that visually
distinguishes nDB from a tabular database.

All three live in a new module `ndb-renderer::viz`. Two of them
(pivot, parallel coords) consume the existing `Table` type produced
by `ndb-slicer`. The hypergraph diagram consumes `&[Record]` directly
because the data model — entities + roles + properties + hyperedges —
doesn't flatten to a `Table` without losing the structure that
matters.

#### 2.10 Pivot table renderer

Goal: tabular display of 4-5 dimensional data via nested row + column
headers. Same shape as Excel / Google Sheets pivot tables.

Design:

  - `pub fn render_pivot(t: &Table, rows: &[usize], cols: &[usize], value: usize, agg: Aggregate) -> String`.
  - `rows` + `cols` are column indexes from `t.headers` to use as row
    keys + column keys respectively. `value` is the column to
    aggregate. `agg` is one of `Aggregate::Sum / Avg / Min / Max / Count`.
  - Output: HTML `<table>` with nested `<th>` headers — multi-row
    header band for `cols`, multi-column row band for `rows`, body
    cells carrying the aggregated value.
  - Empty cells (no rows matching the row × col key combo) render as
    `&nbsp;`.
  - Cell formatting: shared `format_cell` with the flat HTML renderer.
  - No JS — pure HTML.

Tests:

  - 2 row dims × 2 col dims × 1 value (Sum) renders a correct
    cross-tab with totals per cell.
  - Single-dim row + single-dim col matches a simple HTML table with
    rearranged headers.
  - Missing combinations emit empty cells, not 0 (Sum of empty group
    is null, not 0).
  - 3 row dims × 1 col dim renders nested row headers in the right
    order.

Effort: 2 days. Most cost is the nested-header HTML emit + the
multi-key bucket assembly.

#### 2.11 Parallel coordinates renderer

Goal: visualise 5-20 dimensional numeric/ordinal data as polylines
crossing N vertical axes. Standard high-D viz; works well for
classification + outlier detection.

Design:

  - `pub fn render_parallel_coords(t: &Table, opts: ParallelCoordsOpts) -> String`.
  - `ParallelCoordsOpts { width: u32, height: u32, axis_cols: Vec<usize>, color_by: Option<usize> }`.
    Width/height default 1200 × 600.
  - One vertical axis per `axis_cols` index. Numeric columns scale
    linearly between (min, max) observed on that column. Categorical
    columns use ordinal positions (alphabetical sort).
  - Each row becomes a polyline crossing every axis at the row's
    normalised value on that axis. Stroke colour is constant unless
    `color_by` is set; with `color_by` set, the colour bucket is
    derived from the column value (categorical → palette; numeric →
    viridis-ish gradient computed inline).
  - Output: standalone `<html>` document — inline CSS, inline SVG,
    inline JS that highlights the polyline on hover and shows a
    tooltip with the row's values.
  - Null values: skip that row's segment on the affected axis (gap).

Tests:

  - 5-axis table renders 5 axis lines + N polylines (one per row).
  - Numeric-only axis scales correctly (min row touches bottom of
    axis, max row touches top).
  - Categorical axis lays out evenly spaced ticks.
  - `color_by` produces visually distinct stroke colours (assert
    distinct hex codes in the output).
  - Output file opens in headless Chromium without console errors —
    smoke test via `assert!(html.contains("<svg"))` is enough; we don't
    pull in a browser dep for unit tests.

Effort: 3 days. Real cost: axis scaling, colour palette, tooltip JS.

#### 2.12 Hypergraph diagram renderer

Goal: show the hyperedge data model directly. Entities are labelled
nodes; each hyperedge is a polygon (or starburst) connecting its
N role-fillers. This is the renderer that visually proves nDB is
not a tabular database.

Design:

  - `pub fn render_hypergraph(records: &[Record], opts: HypergraphOpts) -> String`.
  - `HypergraphOpts { width: u32, height: u32, type_palette: Option<HashMap<TypeId, &'static str>>, hyperedge_style: HyperedgeStyle, max_nodes: Option<usize> }`.
  - `HyperedgeStyle::Polygon` — draws each hyperedge as a closed
    polygon through its role-filler entity centroids. Good for ≤6
    roles per edge.
  - `HyperedgeStyle::Starburst` — draws a central "hyperedge dot"
    plus radial lines to each role-filler. Good for higher-arity
    edges or visual clutter.
  - Layout: force-directed (Fruchterman-Reingold) computed in Rust
    over ~200 iterations. Inputs are the entities + an adjacency
    derived from co-membership in hyperedges. Output is `(x, y)` per
    entity in `[0, width] × [0, height]`. Deterministic given a seeded
    RNG so the same input always produces the same diagram.
  - Output: standalone `<html>` document — inline CSS, inline SVG,
    inline JS for hover-to-show-properties on both entities and
    hyperedge shapes.
  - Each entity node carries `data-entity-id`, `data-type-id`, and
    `data-properties` (JSON-encoded) attributes — the inline JS pulls
    these into a tooltip on hover.
  - Each hyperedge shape carries `data-hyperedge-id`, `data-roles`
    (JSON-encoded `[{role_id, entity_id}]`), and `data-properties`.
  - `max_nodes` cap: if `records` contains more entities than the cap,
    sample the top-degree subset (most-connected entities first) and
    drop the rest with a warning comment in the HTML. Default 200.
    Without this cap, large databases produce hairball diagrams
    nobody can read.

Tests:

  - Single hyperedge connecting 3 entities renders as a triangle.
  - 4-role hyperedge with `Polygon` style renders as a quadrilateral.
  - Same 4-role edge with `Starburst` style renders a central node +
    4 radial spokes.
  - Force layout is deterministic — same input + seed produces
    byte-identical SVG.
  - `max_nodes = 5` on a 50-entity input keeps the 5 highest-degree
    nodes; output contains exactly 5 entity nodes.
  - Tooltip metadata: `data-properties` JSON on each node parses back
    to the original property set.

Effort: 5-7 days. Force-directed layout + the polygon math + the
hover JS are each non-trivial. Largest single deliverable in v2.1;
also the highest demo value.

## 3. Out of scope — v3 territory

Same boundary as v2.0 plus a few items that don't fit "v2.1 polish":

- True distributed mode (read replicas, geo-replication, raft consensus)
- Write-via-query (extending §12 grammar with mutations)
- IVF / ScaNN vector indexes (HNSW + brute-force suffice for v2-scale workloads)
- gRPC alternative transport
- JS/TS and Go client crates
- **Interactive React/D3 explorer SPA** — full zoom/brush/filter/drill-down
  built on JSON-lines + Arrow IPC + the v2.1 viz outputs. Real second
  codebase, real build pipeline; deserves its own working spec.
- Block-index sidecar encryption (offsets leak some info; defer pending threat-model justification)
- Streaming percentile algorithms (t-digest, GK) — only relevant if real workloads OOM on the naive variant; revisit with data
- SharedEngine fine-grained per-index locks — substantial refactor; v3 platform work
- Pilot deployment integration (Frappe connector, etc.) — separate project once v2.1 ships
- Additional N-dim viz beyond the three in §2.10–§2.12 (small-multiples /
  facet grid, Sankey, radar/spider, heatmap matrix). Each is doable in
  the same Rust → HTML+SVG shape; revisit if the v2.1 three don't
  cover the demo space.

## 4. Sequencing rationale

| Sprint | Deliverables | Effort | Cumulative |
|---|---|---|---|
| 1 | reencrypt, auth dispatch | 1-1.5 wk | 1-1.5 wk |
| 2 | Percentiles, HAVING, time-buckets, multi-sort | 1.5-2 wk | 2.5-3.5 wk |
| 3 | Markdown, JSON-lines, HTML | 0.5-1 wk | 3-4.5 wk |
| 4 | Pivot, parallel coords, hypergraph diagram | 2-2.5 wk | 5-7 wk |

**Critical path:** none between sprints, but inside Sprint 4 the
hypergraph diagram (§2.12) is the largest single item; ship pivot
(§2.10) and parallel coords (§2.11) first so the smaller demos exist
even if §2.12 slips.

Sprints 2, 3, and 4 can interleave — the slicer's percentiles +
multi-key sort pair naturally with parallel coords (which needs sorted
groupings); HTML renderer (§2.9) is a prerequisite shape for the pivot
renderer (§2.10) so do §2.9 first.

**Earliest beta:** end of Sprint 1 if §2.x / §3.x / §4.x slip —
Sprint 1 closes the v2.0 spec gaps which are the only "real" v2.x
bugs. The slicer + renderer + viz work is genuinely additive.

**Most conservative ship date:** ~7 weeks from work start, allowing
for debug + benchmark + docs + release notes + producing the demo
artifact (`docs/v2.1-demo.html` — see §7).

## 5. Success criteria (gates for v2.1 release)

1. **Every v2.0 test still passes.** Plus 80+ new tests for v2.1
   features (rough budget: 8 reencrypt, 6 auth dispatch, 12 slicer,
   6 flat renderer, 15 viz renderer, plus integration).
2. **Clippy clean** with `-D warnings`.
3. **Engine opens any v2.0 database** byte-for-byte, no conversion.
4. **`Engine::reencrypt` crash-tested** — a kill -9 mid-migration
   leaves the database in a recoverable state on next open. Verified
   by a targeted test that injects panic at known boundaries.
5. **Server auth dispatch reads engine on every request** —
   verified by a test that revokes a capability mid-session and asserts
   the next request 403s without a restart.
6. **Every flat renderer handles the same `Table` correctly** —
   round-trip test that builds one Table, runs every flat renderer
   (text / TSV / CSV / Markdown / JSON-lines / HTML), parses each
   output back into a structured form, asserts equality of the
   structured form.
7. **Every viz renderer produces a valid self-contained `.html`** —
   smoke test asserts each output starts with `<!DOCTYPE html>`,
   contains an inline `<svg>` (or for the pivot, an HTML `<table>`),
   has no `<script src=` (no external assets), and parses past 500
   bytes of body content for non-trivial inputs.
8. **Hypergraph layout is deterministic** — same `&[Record]` + same
   seed produces byte-identical SVG. Lets us snapshot-test the
   demo artifact in CI.
9. **`docs/v2.1-demo.html`** — committed artifact built from a
   small seeded biology dataset, demonstrating all three viz
   renderers in one file. Verifies the renderers work end-to-end
   on real data and serves as the canonical demo output.

## 6. Open questions (locked before sprint 1 starts)

- **`Engine::reencrypt` lock semantics.** Hold `&mut Engine` for the
  whole migration, or use a finer-grained "migration mode" flag that
  blocks new writes but lets existing reads complete? Decision: `&mut
  Engine`. Single-process migration is brief enough (seconds to
  minutes) that the simpler API wins. Operators that need long
  migrations can stage them across multiple engine processes via
  external orchestration.

- **Auth dispatch refresh latency.** Should the in-memory `token →
  principal_eid` cache be invalidated on every commit, or only when
  a commit touches a `TYPE_PRINCIPAL` record? Decision: rebuild on
  every `Server::open` and offer `Server::refresh_principals_cache()`
  as a manual hook. Auto-refresh on commit is a future enhancement
  (requires hook plumbing) and not on the critical path — most token
  changes are operator-driven anyway.

- **Percentile interpolation method.** R-7 (linear, NumPy default)
  vs R-6 (linear, Excel default) vs nearest-rank (no interpolation)?
  Decision: R-7. Matches what data scientists already expect from
  pandas / NumPy / DuckDB; nearest-rank is footgun-prone for small N.

- **JSON-lines renderer in `ndb-renderer` or `ndb-arrow`?** Decision:
  `ndb-renderer`. `ndb-arrow` is for binary IPC; JSON-lines is text
  output, same crate as Markdown / TSV / CSV.

- **Multi-column sort API shape.** `Pipeline::sort(Vec<SortKey>)` or
  `Pipeline::sort_by(col, asc).sort_by(col, asc)…` builder chain?
  Decision: `Vec<SortKey>`. Order of keys is explicit + visible; the
  builder pattern obscures key priority.

- **Viz renderers in `ndb-renderer::viz` or a new `ndb-viz` crate?**
  Decision: a new `viz` module inside `ndb-renderer`. Same crate
  avoids dependency-graph fan-out; a sub-module is enough namespace.
  Pivot is conceptually viz too (it's about displaying N-dim data
  in one file) so it lives there even though its output is plain
  HTML rather than SVG.

- **Force-directed layout: hand-rolled or pull in a crate?** Decision:
  hand-rolled Fruchterman-Reingold in ~80 lines. Pulling `petgraph` or
  `force_graph` adds dep weight for one viz feature. Use an inline LCG
  (or `oorandom` if already a workspace dep) for the seeded RNG so the
  layout is deterministic.

- **Parallel coordinates colour palette: viridis or categorical?**
  Decision: both. Numeric `color_by` uses an 8-stop viridis-ish gradient
  computed inline from a hardcoded table; categorical `color_by` uses
  the d3.schemeCategory10 palette inlined. Caller picks via the
  column's value type.

- **Hypergraph diagram for 1k+ entity graphs.** Force-directed layout
  is O(N²) per iteration — fine to ~500 entities, painful above.
  Decision: `max_nodes` cap defaults to 200; callers wanting larger
  diagrams supply their own pre-filtered `&[Record]`. Sampling strategy
  is top-degree-first, deterministic.

- **Pivot renderer empty cells: `0`, empty string, or `&nbsp;`?**
  Decision: `&nbsp;` for visual consistency (the cell still takes
  space); aggregate semantics distinguish "no rows" (null/empty) from
  "rows summed to zero" (rendered as the formatted number).

## 7. v2.1 release readiness

When all 12 deliverables ship + success criteria pass:

- Tag `v2.1.0`
- Build `docs/v2.1-demo.html` — single self-contained file
  demonstrating pivot + parallel coords + hypergraph diagram on a
  small seeded biology dataset. Committed alongside the release so
  reviewers can open it locally without running the server.
- Write `2026-XX-XX-v3-working-spec.md` covering distributed mode +
  write-via-query + gRPC + JS/Go clients + the **interactive React/D3
  explorer SPA** (the natural successor to v2.1's static viz outputs,
  consuming the same Arrow IPC + JSON-lines pipes).
- Update README to reflect v2.1 capabilities; embed a screenshot of
  the hypergraph diagram alongside the existing prose.
- Frappe pilot deployment can begin (using v2.1 as the engine; v2.0
  also viable but v2.1's analytics + viz surface is the better
  starting point for any product built on top).
