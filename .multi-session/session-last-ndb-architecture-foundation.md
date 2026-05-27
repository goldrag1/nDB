## Session 2026-05-27 (fourth turn) — query language scaffolding: spec → wire AST → parser → resolver

Started from v1.2.0 (262 Rust + 12 Python = 274 tests) toward the §17.1
query-language deliverable — the dominant remaining piece. This session
landed the first four of the eight steps needed (spec, wire AST, parser,
resolver). The planner / executor / `/query` route / client surfaces
remain for the next session.

**7 new commits**, **+80 tests** (262 → 342 Rust). Workspace clippy clean
with `-D warnings`. Branch `main` ready to push.

### What landed this turn

| SHA       | Subject |
|-----------|---------|
| `ae0fe30` | spec(query-language): close §12.9 open sub-questions; lock v1 grammar + AST + semantics |
| `010c652` | fix(test): hoist const decls to top of traverse_route_walks_2_hops (clippy 1.95 regression on main) |
| `f972ba8` | feat(engine): wire_query — QueryRequest/Response AST + 16 round-trip tests |
| `efc1285` | spec(query-language): lock hyperedge semantics — partial match, role-vs-property (Option A) |
| `6008d77` | feat(query): ndb-query crate — lexer + recursive-descent parser (TDD) |
| `3ebd77e` | feat(query): resolver — NameQuery → wire QueryRequest via dictionaries |

The query-language working spec lives at
`docs/superpowers/specs/2026-05-27-query-language.md`.

### Locked design decisions for query language (in addition to §12 of the parent spec)

| Concern | Decision | Source |
|---|---|---|
| Surface syntax | SQL-ish pattern functions, `type(role: term, ...) as ?var`. Chosen over TypeQL `$x isa`, bracket-record, YAML-block — scales cleanly at high arity via role labels. | working spec §2.1, user A/A/A |
| Self-bind | `as ?var` suffix; `id:` is NOT a reserved key. Replaces §12.6 examples that used `id:` magically. | spec §2.3, §2.4 |
| Operator precedence | `not` > comparisons > `and` > `or`. Comparisons non-associative (`a < b < c` → ChainedComparison error). No arithmetic in v1 — push math into slicer. | spec §3.1 |
| Recursion suffix position | BEFORE `(` (per §12.6 examples like `contains*(...)`). Parent-spec EBNF placed it after `)`; corrected inline. | spec §3 |
| Recursion semantics | Single query-start snapshot for the entire closure. Visited-set cycle protection. Default max_depth=64. Loud error on cap (never silent truncate). | spec §5.3 |
| Partial role match | Unnamed roles are wildcards. `_` placeholder for fresh anonymous variable in patterns; disallowed in `where`. | spec §5.7 |
| Same-variable unification | Repeated variable in a single pattern unifies — no join needed. | spec §5.7 |
| Role-vs-property name resolution | Option A (overload by name). Resolver decides per dictionary; same name as both → ambiguous_name error. Preserves §12.6 syntax verbatim. | spec §5.7 |
| PropertyFilter RHS | `term: Term` (var OR literal), not literal-only — needed for `customer(name: ?n)` bind-to-variable shape. | spec §4, amended this session |
| Wire AST id-based | Type/role/property as u32 dictionary slots. Resolver maps names → ids by walking a Dictionaries snapshot of `Engine::snapshot_iter`. | spec §2.2, §4 |
| v1 is READ-ONLY | Writes through `/commit`. Writing through query syntax adds read-set tracking + conflict detection to executor; deferred to v2. | spec §1, §9 |
| NL-to-AST | Engine grammar is the only input path. NL wrappers are a client/SDK concern. Engine stays deterministic + offline-capable. | spec §2.5 |
| Tagged-union conventions | `#[serde(tag = "kind", rename_all = "snake_case")]` for Pattern / Term / Expr / Recursion. `AsOf` is untagged — field name IS the discriminator. Matches existing `JsonRecord`. | spec §4.2 |
| Anonymous in pattern | Each `_` becomes a fresh `__anon_N` variable (thread-local counter) so multiple `_`s in the same pattern don't unify. | resolver |

### Workspace shape after this session

```
crates/
├── ndb-engine             # +wire_query module (~700 LOC, 17 tests)
├── ndb-server             # +clippy hoist fix
├── ndb-cli                # unchanged
├── ndb-mcp-server         # unchanged
├── ndb-slicer             # unchanged
├── ndb-renderer           # unchanged
├── ndb-arrow              # unchanged
├── ndb-index-vector-hnsw  # unchanged
├── ndb-client-rust        # unchanged
└── ndb-query              # NEW — lexer + parser + resolver
                           # ~2000 LOC, 76 tests + 1 doctest
```

### Bugs caught + fixed inline this turn

1. **Clippy 1.95 `items_after_statements` lint** broke the existing `traverse_route_walks_2_hops` test on main. Pre-existing regression — v1.2.0 shipped clippy-clean, but a newer Rust/clippy version made `const TYPE_X: u32 = ...;` interleaved with `let` lines a hard error. Fixed by hoisting consts to the top of the test function in commit `010c652`. Worth a watch on the next bench/server change — this lint may fire elsewhere.
2. **Parent-spec EBNF placed recursion suffix AFTER `)`** but every §12.6 example uses suffix AFTER type-name + BEFORE `(` (`contains*(parent: ..., child: ...)`). Corrected inline in the working spec; parser implements the example-correct form.
3. **PropertyFilter.value (JsonValue, literal-only)** couldn't express `customer(name: ?n)` — variable bind to property value. Amended the wire AST in the same commit before any wire consumers existed (resolver was the first consumer; tests updated together). No external clients affected.
4. **Awk RSTART/RLENGTH ordering bug in my own test-count script** — the second `match()` overwrote RSTART before the first `substr()` ran, so the script reported `passed=341 failed=1` when actually `passed=342 failed=0`. Pure tooling bug, no code impact, fixed by using awk's array-capture form `match($0, /(...)/ , arr)`.

### §17.1 status after this session

**Shipped this session:**
- Query language §12 working spec (closes §12.9 open sub-questions) ✅
- Query language wire AST (`QueryRequest` / `QueryResponse` in `ndb-engine::wire_query`) ✅
- Query language parser (`ndb-query` crate — lexer, AST, recursive-descent parser, span-based errors) ✅
- Query language resolver (`ndb-query::resolve` — Dictionaries snapshot + name→id mapping + entity-vs-hyperedge classification) ✅

**Still to land before query language is end-to-end usable:**
- Planner: smallest-cardinality-first join order. Output: executable plan tree. Picks per-atom primitive from `lookup_by_external_key` / `property_lookup` / `property_range` / `hyperedges_by_type` / `hyperedges_for_entity`. ~2-3 days of work.
- Executor: walks plan tree, threads variable bindings, materialises rows. Includes recursive-pattern BFS with visited-set + depth cap. ~3-5 days.
- `/query` route in `ndb-server`: same auth + audit + ReBAC as existing routes; `Capability::Read`; round-trip test via TCP loopback. ~1 day.
- Client surfaces: `.query(req)` on `ndb-client-rust` + Python `client.query` + CLI `ndb query` subcommand reading from stdin. ~1 day.

After those four steps, the query language is usable end-to-end and the
biology bench dashboard can exercise it as a fifth tab.

### Other §17.1 deliverables not started this turn (parked)

- Per-type retention policies (Audited / Versioned / LatestOnly) — task #8
- Serializable Snapshot Isolation — task #9
- Time-travel `as of T` via wire — task #10 (engine supports internally; route param + AST field already in this session's wire AST as `as_of`)
- Streaming query cursors `/iter_stream` / `/query_stream` — task #11
- Change subscription `/subscribe` — task #12
- Mmap'd SSTable read paths — task #13
- Validation driven by metadata hyperedges — task #14
- Real-world pilot + Neo4j comparison + docs site — adoption work, parked

### Next session entry point

The natural next step is the planner. It targets the wire `QueryRequest`
(which is what the resolver produces) and outputs a `Plan` tree whose
nodes are engine-primitive calls. Algorithm locked in working spec §7:

1. Per-atom cardinality estimate using available indexes.
2. Seed with the smallest-cardinality atom; pick the matching engine
   primitive (`property_lookup` if B-tree exists, else `hyperedges_by_type`,
   etc.).
3. Greedy join order — pick the next atom by max-shared-vars,
   ties broken by cardinality.
4. Push down single-atom `where` predicates to scan time; cross-atom
   ones run at join time.
5. `limit` push-down where the join is on a unique constraint.

The planner can live in `ndb-engine::query_plan` (it needs engine
primitives + index stats) or in a new `ndb-engine` sub-module. Suggest
`crates/ndb-engine/src/query_plan.rs` since it bridges wire AST →
plan tree, and the plan tree's nodes are engine-primitive calls.

After the planner, the executor walks plan tree → result rows. The
recursive-path executor needs special handling (BFS with visited set
+ depth cap); start with non-recursive plans first to land a v0
end-to-end, then add recursion.

`Engine::snapshot_iter` is what callers feed `Dictionaries::from_records`
to get a snapshot dictionary. v2 will cache Dictionaries on the engine
so this isn't an O(N) walk per query.

### Evolution score this turn

- 7 new commits in nDB repo
- 1 new crate (`ndb-query`)
- 1 wire module added (`ndb-engine::wire_query`)
- 1 spec amendment (parent §12.9 closure + new working spec)
- +80 tests (262 → 342 Rust)
- 0 cross-project rules promoted (everything here is project-specific
  to the query-language design)

---

## Session 2026-05-27 (third turn) — v1.2.0 — multi-hop traversal + indexed routes + biology bench dashboard

Built on top of v1.1.0 to make nDB usable from real applications without N+1
round-trips and to provide a benchmark surface that exercises every index.

**11 new commits** (since v1.1.0), **+19 tests** (243 → 262 Rust + 12 Python).
Workspace clippy clean. **v1.2.0 tagged + pushed + released**.

### What landed this turn (in nDB repo)

| SHA       | Subject |
|-----------|---------|
| `a8ce398` | feat(server): indexed query routes — /lookup, /vector_search, /property_lookup, /property_range |
| `911fafb` | feat(client-rust): ndb-client-rust — reusable Rust HTTP library + CLI rewrite |
| `311bf66` | feat(server): --bench-mode flag — pre-register simple workload schema |
| `c15b157` | feat(server): biology schema in --bench-mode |
| `a9fa2bd` | release: nDB v1.2.0 — +/traverse, +biology bench schema, +ndb-client-rust |

`v1.2.0` tag: <https://github.com/goldrag1/nDB/releases/tag/v1.2.0>

### What landed in `/home/long/long/rust/` (separate workspace, not git-tracked)

A live benchmark dashboard at http://127.0.0.1:8766/ with four tabs:

1. **Prime Race** (untouched from before — Rust / ASM / Python prime counting)
2. **nDB Bench** — Rust client vs Python client, simple workload
3. **🧬 Biology Bench** — Rust client vs Python client, pharmacogenomic workload
4. **🐘 Rust+nDB vs Python+PostgreSQL** — head-to-head on biology workload

Files:
- `rust/ndb-bench/src/main.rs` — Rust bench, biology + simple modes, hub-routed fanout
- `rust/python/ndb_bench.py` — Python bench, same modes
- `rust/python/pg_bench.py` — Python+psycopg3 against PG with pgvector
- `rust/server/src/main.rs` — orchestrator with `/ndb_bench` SSE + `/ndb_bench/inspect` proxy + parked-children `BenchState`
- `rust/web/index.html` — 4-tab dashboard with scaling-trend chart on PG tab

### Locked v1.2 decisions (in module preambles)

| Concern | Decision | Module |
|---|---|---|
| Multi-hop traversal | Server-side BFS via `POST /traverse` — single round-trip with per-hop type filters | `ndb-server/src/lib.rs::handle_traverse` |
| Traversal frontier | `HashSet<EntityId>` dedup, BFS layer-by-layer; reads each hyperedge to get role bindings | same |
| Indexed query route gating | All four indexed query routes plus `/traverse` mapped to `Capability::Read` | `required_capability()` |
| `--bench-mode` schema | Two pre-registered workloads (simple users + biology drug/protein/disease/publication) co-exist | `ndb-server/src/main.rs` |
| Biology schema constants | TYPE 100-103 entities, 200-202 hyperedges, PROP 30-41, ROLE 10-16 — pub from `main.rs` for clients | same |
| Vector cap on `/vector_search` | `MAX_VECTOR_K = 1000` — enforced server-side, returns 400 on bigger k | `ndb-server/src/lib.rs` |
| `/iter` semantics at scale | Bench programs skip iter past N=50k client-side; server still serves it but materialises full set | `ndb-bench/src/main.rs`, `ndb_bench.py`, `pg_bench.py` |
| Benchmark fanout shape | Hub routing: every 20th protein slot is a "hub", ~50% of edges land there → 20× heavy-tail | `hub_idx()` in all three benches |

### Bench measurements observed this turn (commodity laptop)

Biology workload, Rust+nDB vs Python+Postgres, scaling trend:

| N | Rust+nDB | Python+PG | Winner | Ratio |
|---|---|---|---|---|
| 400 | 183 ms | 122 ms | postgres | 1.50× |
| 2,000 | 860 ms | 745 ms | postgres | 1.15× |
| 10,000 | ~8 s | ~10 s | **rust+nDB** | 1.30–1.40× |
| 50,000 | ~42 s | ~75 s | **rust+nDB** | **1.80×** |

Crossover ≈ N=10k on this machine. nDB's adjacency-walk traversal pulls ahead
as N grows; PG's per-query baseline (libpq + planner) advantage fades.

3-hop traversal at N=2,000: nDB **2.00×** PG (vs the 2-hop 1.29×).
3-hop with hub fanout will show wider gaps at production-shape N.

### Bugs caught + fixed inline this turn

1. **clippy `match → let-else`** — bumped on first compile of `/traverse` handler; trivial fix but worth noting that v1.95 clippy is more aggressive.
2. **8 orphaned `ndb-server` children across dashboard restarts** — the `/home/long/long/rust/server` doesn't install a SIGINT handler, so its `BenchState::teardown` never runs on shutdown. Children get adopted by init. Documented as a follow-on; recovery is `pkill -af 'ndb-server --bench-mode'` by PID excluding own shell.
3. **Tokio `Child::kill().await` leaves a zombie** — kill sends SIGKILL but doesn't `wait()`. The PID lingers as `<defunct>` until the parent process exits. Cost: one process-table entry, no resources.
4. **Self-kill `pkill -f` re-triggered** — already in `shell-quirks.md`; my own bash shell argv contained the literal `rust/target/release/ndb-bench` because of how the harness eval'd it. Mitigated by enumerate-PIDs-then-kill pattern (rule already exists).
5. **Section-tag balance** — inserting big HTML blocks via Edit twice in a row over-closed `</section>` — both times caught by `grep -nE '^</?section'` post-edit. Worth doing every time after a large HTML insertion.

### §17.1 status after v1.2.0 (honest read)

**Shipped:**
- Storage core + 6 mandatory indexes ✅
- Slicer + renderer ✅
- Validation (runtime) ✅
- Brute-force + HNSW vector indexes ✅
- Rust CLI + Rust library + Python client + Arrow IPC ✅
- MCP server ✅
- Wire protocol + bearer-token + multi-principal ReBAC + TLS + audit log ✅
- At-rest encryption primitives (WAL/SSTable wiring deferred) ✅
- Indexed query routes + multi-hop /traverse + bench-mode schema ✅

**Spec §17.1 deliverables not yet built:**
- **Query language (§12) — the dominant missing piece**. Datalog-influenced pattern-match DSL, structured AST wire format, optional Rust embedded DSL.
- Per-type retention policies (Audited / Versioned / LatestOnly)
- Hot/cold SSTable tiering
- Serializable Snapshot Isolation (SI is shipped; SSI is not)
- Time-travel `as of T` syntax exposed via wire (engine supports snapshot reads internally)
- Streaming query cursors / change subscription (`subscribe`)
- Mmap'd SSTable files (still BufReader)
- Validation driven by metadata hyperedges (today runtime-only)
- Block index sidecar `<seq>.idx` (deferred to v2 per design)
- Real-world pilot + Neo4j comparison + documentation site

### Next session priorities (for the v1-completion session)

The top item is the **query language (§12)**. Everything else is smaller and
can be batched. A separate "start next session" prompt is being prepared
alongside this session-last.

### Evolution score this turn

- 11 new commits in nDB repo
- 1 new tag (v1.2.0) + GitHub release
- +19 tests (243 → 262 Rust + 12 Python = 274 total)
- 1 new live benchmark dashboard (4 tabs, 1 SSE orchestrator, 1 inspect proxy, scaling-trend chart)
- 2 cross-project rules promoted (see `.pending-promotions.md`)

---

## Session 2026-05-27 — nDB v1 storage core + companion crates + wire + AI bridge

### Đã làm (initial v1.0.0 release)

Implemented the v1 storage core end-to-end AND the full v1 companion-crate
stack. Starting from a documentation-only repo, this session shipped a
working hypergraph database with HTTP wire protocol, CLI client, MCP server
for AI agents, CPU slicer, text/CSV renderer, and all six mandatory indexes.

`README.md` written at the repo root explaining the workspace, the wire
protocol, the on-disk layout, and what's shipped vs. deferred to v2.

## Session 2026-05-27 (continuation) — §17.1 closing items

Built on top of v1.0.0 to close the four remaining §17.1 deliverables.
**8 new commits**, **+50 tests** (193 → 243 Rust + 8 Python). Clippy clean
across the workspace.

### Commits added this turn

| SHA       | Subject |
|-----------|---------|
| `dc3559d` | feat(arrow): ndb-arrow crate — Arrow IPC interop (§17.1) |
| `f0a950f` | feat(server): audit log — append .audit.jsonl per request (§13.5) |
| `c7154d5` | feat(server): ReBAC capabilities — per-route principal-gating (§13.2) |
| `78046ed` | feat(server): TLS termination via rustls (§13.3) |
| `376e754` | feat(engine): at-rest encryption primitives — Cipher + EncryptedFile (§13.4) |
| `6cce20a` | feat(mcp): ReBAC capabilities + audit log for stdio tool calls (§13) |
| `a7fae26` | feat(index): ndb-index-vector-hnsw — HNSW ANN over instant-distance (§14.2) |
| `cc2bbe3` | feat(python): clients/python — pure-Python HTTP client (§17.1) |
| `c9122dc` | chore: gitignore Python bytecode caches |

### §17.1 deliverable checklist — full v1 status

| Deliverable                            | Status |
|----------------------------------------|--------|
| Engine + 6 mandatory indexes           | ✅ shipped |
| nDB-slicer (CPU projection/aggregation)| ✅ shipped |
| nDB-renderer (2D text/TSV/CSV)         | ✅ shipped |
| Validation engine                      | ✅ shipped (runtime registration; metadata-hyperedge-driven still deferred to v2) |
| Brute-force vector index               | ✅ shipped |
| **HNSW vector index**                  | ✅ shipped (`ndb-index-vector-hnsw`, opt-in plugin) |
| nDB-client-rust (CLI)                  | ✅ shipped |
| nDB-cli                                | ✅ shipped (`ndb` binary) |
| nDB-mcp-server                         | ✅ shipped (stdio JSON-RPC) |
| Wire protocol (HTTP + JSON + JSONL)    | ✅ shipped |
| **TLS via rustls**                     | ✅ shipped (`--tls-cert` / `--tls-key`) |
| Bearer-token auth                      | ✅ shipped |
| **ReBAC capability gating**            | ✅ shipped (server routes + MCP tools) |
| **Audit log (.audit.jsonl)**           | ✅ shipped (shared by server + MCP) |
| **At-rest encryption primitives**      | ✅ shipped (`Cipher`, `EncryptedFile`); WAL/SSTable wiring deferred |
| **nDB-client-python**                  | ✅ shipped (`clients/python/`, pure-stdlib) |
| **Arrow IPC interop**                  | ✅ shipped (`ndb-arrow` crate) |
| Block index sidecar (`<seq>.idx`)      | ❌ deferred to v2 |
| Snapshot-aware compaction              | ❌ deferred to v2 |
| Query language                         | ❌ §12.9 grammar still open; needs own session |
| Validation driven by metadata hyperedges | ❌ deferred to v2 |
| Capability hyperedges as ReBAC store   | ❌ deferred to v2 (today: in-memory `principals.json`) |

### Workspace shape after this session

```
crates/
├── ndb-engine             # +encryption module (~600 LOC, 14 tests)
├── ndb-server             # +audit + principals + TLS (~1100 LOC, 16 tests)
├── ndb-cli                # unchanged
├── ndb-mcp-server         # +principal gating + audit (11 tests)
├── ndb-slicer             # unchanged
├── ndb-renderer           # unchanged
├── ndb-arrow              # NEW — Arrow IPC bridge (~700 LOC, 12 tests)
└── ndb-index-vector-hnsw  # NEW — HNSW plugin (~440 LOC, 13 tests)

clients/
└── python/                # NEW — pure-Python HTTP client (8 tests; 3 are gated on NDB_PYTHON_SMOKE=1)
```

### Locked v1 decisions added this session

| Concern | Decision | Module |
|---|---|---|
| Arrow schema shape | Denormalised: one column per `(record_kind, type_id, property_id)` + identity columns + roles `List<Struct{role_id, entity_id}>` | `ndb-arrow/src/lib.rs` |
| Arrow column dtype binding | First observed value picks the dtype; tag conflict → `TypeMismatch`; Null is compatible with any dtype | `ndb-arrow/src/lib.rs` |
| Arrow dictionary handling | `TypeName`/`RoleName`/`PropertyKey` records filtered out of rows; exposed via `build_dictionaries(records) -> Dictionaries` | `ndb-arrow/src/lib.rs` |
| Decimal in Arrow | Widens to Float64 (lossy past ~15 digits; v2 → Arrow native `Decimal128`) | `ndb-arrow/src/lib.rs` |
| ReBAC granularity | Coarse `Capability` enum (Health/Read/Iter/Commit/Flush/Compact/Admin) + Principal { name, capabilities: BTreeSet } | `ndb-server/src/lib.rs` |
| Principal storage | In-memory map loaded from `<db>/.principals.json`; v2 migrates to capability hyperedges | `ndb-server/src/lib.rs` |
| Audit log path | `<db>/.audit.jsonl`, JSON-per-line, synchronous flush, best-effort (write failure → stderr) | `ndb-server/src/lib.rs` |
| Audit fields | ts_us, principal, method, path, status, optional tx_id, optional failure | `ndb-server/src/lib.rs` |
| TLS stack | rustls 0.23 + ring; TLS 1.2/1.3; PEM cert + PKCS#8/PKCS#1/SEC1 keys | `ndb-server/src/lib.rs` |
| TLS API shape | `with_tls(Arc<ServerConfig>)` + `with_tls_pem(cert, key)` + `bind_tls`/`run_tls` paths; plain TCP unchanged | `ndb-server/src/lib.rs` |
| MCP gating | Optional `with_principal(Principal)`; tool→capability map; `NDB_MCP_PRINCIPAL` env on the binary | `ndb-mcp-server/src/lib.rs` |
| Cipher algorithm | AES-256-GCM (only — agility deferred to v2 KMS plugin) | `ndb-engine/src/encryption.rs` |
| Encrypted-file framing | Plaintext header (magic + version + chunk_size) + chunked AEAD (4 KiB plaintext per chunk by default); each chunk has its own random nonce | `ndb-engine/src/encryption.rs` |
| Key sourcing | `NDB_ENC_KEY` env (hex-encoded 64 chars) or `Cipher::from_raw_key` programmatically | `ndb-engine/src/encryption.rs` |
| HNSW backend | `instant-distance` 0.6 (pure safe Rust, zero unsafe) — chosen over `hnsw_rs` for cleanliness | `ndb-index-vector-hnsw/src/lib.rs` |
| HNSW rebuild policy | Lazy: `apply()` marks bucket dirty; `search()` rebuilds on first call or metric flip | `ndb-index-vector-hnsw/src/lib.rs` |
| HNSW default params | `ef_construction=100`, `ef_search=100`, seed=0; `BuilderConfig` exposed for tuning | `ndb-index-vector-hnsw/src/lib.rs` |
| Python transport | Stdlib `urllib` — zero non-stdlib deps in the base install; `pyarrow` only via `[arrow]` extra | `clients/python/ndb_client/client.py` |
| Python surface | Mirrors `ndb` CLI: health/commit/read/iter/flush/compact + lookup_by_key/vector_search/property_lookup/property_range (last four client-side over `/iter` until server adds routes) | `clients/python/ndb_client/client.py` |

### Bugs caught + fixed inline this session

1. **Arrow `ListBuilder<StructBuilder>` builds nullable inner field, not non-null.** Schema declared `nullable: false` for the roles-list inner field; built array reported `nullable: true`; `RecordBatch::try_new` rejected. Fix: declare `Field::new("item", DataType::Struct(...), true)` so the schema matches what the builder produces.
2. **Server I/O refactor for TLS — `&mut TcpStream` → `&mut dyn Write`.** The dispatch chain previously held a concrete `TcpStream`; TLS needs a wrap. Generalised every handler signature; the plain-TCP path keeps its `try_clone` for the BufReader, the TLS path uses `rustls::StreamOwned` with the same parse_request now generic over `Read`.
3. **`gh repo create / gh pr` not used — direct git push to `origin main` (single-maintainer repo).** Just commits, no PRs. Same convention as the v1 arc.
4. **Encrypted file header MUST be plaintext.** Reader has to recognise the file as encrypted before it has a chance to decrypt. Magic + version + chunk_size live outside the AEAD envelope (sniffable + tamper-detectable via downstream chunk auth, not via the header).
5. **HNSW's `instant-distance` doesn't support incremental insert.** Decided lazy-rebuild over forking the crate. Documented the build-many-search-many ergonomics in the module preamble.
6. **Python client's `lookup_by_key` / `vector_search` / `property_lookup` / `property_range` are client-side scans over `/iter`.** The server has the indexes but doesn't expose routes for them yet — the client surface anticipates them (the API doesn't change when routes land in v1.1).
7. **Audit-log MCP integration cleanly shares the `AuditLog` + `AuditEntry` types from `ndb-server`.** Added a tiny `ndb-server`-as-dep edge from `ndb-mcp-server` rather than duplicating the audit machinery.

### Next session priorities (when work resumes)

1. **Wire `EncryptedFile` into WAL and SSTable I/O paths.** The primitives are ready; recovery / compaction interaction needs careful design. Estimated 1 week to land cleanly. Per-DB `.encryption` marker file to record the magic so MANIFEST + CURRENT can refuse to open an encrypted DB without the key.
2. **Server-side routes for `/lookup`, `/vector_search`, `/property_lookup`, `/property_range`.** The Python client (and any future client) currently does client-side scans. Adding routes is mostly mechanical — the engine methods already exist (`Engine::lookup_by_external_key` etc.).
3. **Block index sidecar (`<seq>.idx`).** Make `SSTableReader::find` O(log N). Touches sstable writer (emit sidecar at finish), reader (mmap + binary search), MANIFEST (list sidecar paths). Substantial change.
4. **Query language (§12).** Datalog-influenced pattern matching. Spec §12.9 grammar still open. Needs its own focused session.
5. **Snapshot-aware compaction.** Track oldest live snapshot; only drop versions older than it.
6. **Capability hyperedges as the persistent ReBAC store.** Migrate `principals.json` → hyperedges of a reserved CAPABILITY type. v1 in-memory shape is the shadow of that future model.

### Bench-of-bench verified manually this session

- `cargo run -p ndb-server -- --path /tmp/x --tls-cert ... --tls-key ...` (TLS bind + curl --cacert)
- Python `python3 -m unittest tests.test_smoke -v` with `NDB_PYTHON_SMOKE=1` against a freshly-spawned server — 8/8 pass
- HNSW agreement with brute force on a 200-vector deterministic dataset (top-1 must match — passes)
- Audit log inspection: `cat /tmp/x/.audit.jsonl` after a series of commits and a 404 — JSON-per-line as advertised

### Evolution score for this session

- 8 new commits + 1 chore
- 2 new crates (`ndb-arrow`, `ndb-index-vector-hnsw`)
- 1 new client (`clients/python/`)
- +50 tests (193 → 243 Rust + 8 Python; total 251)
- Spec §13.4 and §14.2 amended to reflect shipped state
- 0 cross-project rules promoted (every pattern here is project-specific to nDB's v1 surface)
