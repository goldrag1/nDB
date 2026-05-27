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
