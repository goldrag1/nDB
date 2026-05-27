# nDB v2.1 — Working Spec

> **Status:** Drafted 2026-05-27, opens immediately after v2.0.0 shipped.
> Locks the v2.1 release identity, scope, sequencing, and success
> criteria. v2.0 successor; tighter scope, shorter sprint.

## 1. Identity

**v2.1 is the "finish v2.0" release.** v2.0 closed every v1 limitation
the working spec listed, but landed two follow-ons of its own
(`Engine::reencrypt` deferred from §6 of the v2.0 spec, server auth
dispatch still reading the in-memory cache instead of the engine).
v2.1 closes both, and uses the remaining sprint room to broaden the
analytics surface — `ndb-slicer` is the one user-facing API where
"correct but minimal" still describes the v2.0 state.

This is **NOT** a platform release. No new transports, no distributed
mode, no §12 grammar additions. The on-disk format is unchanged;
v2.0 databases open in v2.1 byte-for-byte and the reverse holds
modulo the new analytics being unused.

Critical-path constraint: v2.1 must ship **before** v3 work begins, so
the v3 working spec can focus on platform-shape decisions (distributed
mode, write-via-query, gRPC, JS/Go clients) without inheriting v2-era
caveats.

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

## 3. Out of scope — v3 territory

Same boundary as v2.0 plus a few items that don't fit "v2.1 polish":

- True distributed mode (read replicas, geo-replication, raft consensus)
- Write-via-query (extending §12 grammar with mutations)
- IVF / ScaNN vector indexes (HNSW + brute-force suffice for v2-scale workloads)
- gRPC alternative transport
- JS/TS and Go client crates
- Block-index sidecar encryption (offsets leak some info; defer pending threat-model justification)
- Streaming percentile algorithms (t-digest, GK) — only relevant if real workloads OOM on the naive variant; revisit with data
- SharedEngine fine-grained per-index locks — substantial refactor; v3 platform work
- Pilot deployment integration (Frappe connector, etc.) — separate project once v2.1 ships

## 4. Sequencing rationale

| Sprint | Deliverables | Effort | Cumulative |
|---|---|---|---|
| 1 | reencrypt, auth dispatch | 1-1.5 wk | 1-1.5 wk |
| 2 | Percentiles, HAVING, time-buckets, multi-sort | 1.5-2 wk | 2.5-3.5 wk |
| 3 | Markdown, JSON-lines, HTML | 0.5-1 wk | 3-4.5 wk |

**Critical path:** none — every sprint is independent. Sprints 2 and
3 can interleave (slicer + renderer pair well; ship a feature + its
renderer in the same commit when natural).

**Earliest beta:** end of Sprint 1 if 2.x / 3.x slip — Sprint 1 closes
the v2.0 spec gaps which are the only "real" v2.x bugs.

**Most conservative ship date:** ~5 weeks from work start, allowing for
debug + benchmark + docs + release notes.

## 5. Success criteria (gates for v2.1 release)

1. **Every v2.0 test still passes.** Plus 50+ new tests for v2.1
   features (rough budget: 8 reencrypt, 6 auth dispatch, 12 slicer,
   6 renderer, plus integration).
2. **Clippy clean** with `-D warnings`.
3. **Engine opens any v2.0 database** byte-for-byte, no conversion.
4. **`Engine::reencrypt` crash-tested** — a kill -9 mid-migration
   leaves the database in a recoverable state on next open. Verified
   by a targeted test that injects panic at known boundaries.
5. **Server auth dispatch reads engine on every request** —
   verified by a test that revokes a capability mid-session and asserts
   the next request 403s without a restart.
6. **All four renderers handle the same `Table` correctly** — round-trip
   test that builds one Table, runs every renderer, parses each output
   back into a structured form, asserts equality of the structured form.

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

## 7. v2.1 release readiness

When all 9 deliverables ship + success criteria pass:

- Tag `v2.1.0`
- Write `2026-XX-XX-v3-working-spec.md` covering distributed mode +
  write-via-query + gRPC + JS/Go clients
- Update README to reflect v2.1 capabilities
- Frappe pilot deployment can begin (using v2.1 as the engine; v2.0
  also viable but v2.1's analytics surface is the better starting
  point for any product built on top)
