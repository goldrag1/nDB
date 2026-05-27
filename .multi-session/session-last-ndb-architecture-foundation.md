## Session 2026-05-27 — nDB Architectural Foundation Complete

### Đã làm

Brainstormed and committed the full architectural foundation for nDB, an n-dimensional hypergraph database engine in Rust.

**Deliverables (committed on `develop` branch):**

- `docs/superpowers/specs/2026-05-27-nDB-hypergraph-design.md` — full architectural design, ~1810 lines, 19 sections. Covers everything from byte-level record layouts to v1/v2/v3/v4+ roadmap.
- `docs/nDB-whitepaper.md` — public-facing distillation, ~470 lines, with diverse domain examples (biology, chemistry, construction engineering, business, biomedical).

**16 commits on `develop`, clean working tree, no Rust code yet.**

### Quyết định quan trọng (all locked, in design spec)

| Concern | Decision |
|---|---|
| Data model | Hyperedge-native (entity / hyperedge / property); N-ary facts as atomic primitives, no reification |
| Schema | Metadata hyperedges; not a separate engine primitive ("schema" is shorthand) |
| Namespace | None (rejected as SQL artifact); config attaches per-type / per-transaction / per-entity |
| Slicer + Renderer | Companion crates, not engine layers |
| Storage | Custom binary, append-only LSM, `.ndb` file extension; database is a directory not a single file |
| Identifiers | UUID v7 internal + pluggable external lookup keys |
| Transactions | MVCC SI/SSI per transaction (caller-specified) |
| Query language | Datalog-influenced pattern matching, SQL-like surface, JSON AST wire format; **retrieval only** (no aggregation in engine) |
| Compute | Slicer crate does aggregation/sort/math; engine retrieves |
| Wire protocol | HTTP + JSON requests + JSONL streaming responses + embedded mode |
| Indexes | Framework + plugin model; 6 mandatory built-in in v1; hardware-neutral (CPU/GPU/FPGA) |
| Concurrency | Single-writer + batching, lock-free MVCC reads, Tokio async |
| GPU support | v1 prep (batch APIs + mmap-friendly storage); v2 CUDA plugins; v3 cross-platform |
| Containment | Standard hyperedges with `contains` relation; per-hyperedge `lifecycle` (cascade/orphan/restrict, default cascade) |
| Recursive queries | Datalog-style suffix syntax (`relation*`, `relation+`, `relation?`, `relation{n,m}`) |
| Security | Bearer tokens + TLS + ReBAC via capability hyperedges + free audit via MVCC |

### V1 Deliverables (after pull-forward from v2 — Section 17.1)

Engine + 6 mandatory built-in indexes (entity-by-ID, hyperedge-by-ID, lookup-key reverse, adjacency list, hyperedge-type clustering, schema-driven property B-tree). Plus these companion crates:

- **nDB-slicer** (CPU projection + aggregation)
- **nDB-renderer** (2D: table, scatter, pivot, bar/line/area)
- **Validation engine** (constraint enforcement, strict/soft modes)
- **nDB-index-vector-cpu** (HNSW similarity search)
- **nDB-client-rust** + **nDB-client-python** (wire-protocol clients)
- **nDB-cli** (interactive REPL + admin tooling)
- **nDB-mcp-server** (Model Context Protocol — AI agent integration)
- **Arrow IPC interop** (Polars / pandas / DuckDB zero-copy)
- Security baseline (bearer tokens + TLS + ReBAC capabilities + free audit + filesystem encryption)

### Trạng thái hiện tại

Repo at `/home/long/long/nDB-ndimemsion-database` has 16 commits on `develop`, clean working tree. `main` and `develop` branches exist; no remote configured. No Rust code yet — purely documentation. Ready to begin implementation.

### Next Session Task

**Begin v1 storage core implementation.** Use Opus 4.7 throughout (NOT opusplan — this is direct implementation, not planning). User explicitly said no implementation plan needed — work directly from the design spec.

**Concrete starting sequence:**

1. Read both design documents end-to-end:
   - `docs/superpowers/specs/2026-05-27-nDB-hypergraph-design.md` (authoritative)
   - `docs/nDB-whitepaper.md` (narrative companion)
2. Set up a Cargo workspace at the repo root; create the `ndb-engine` crate.
3. Implement the record layouts from Section 11.2 with serialization round-trip tests:
   - HyperEdgeRecord, EntityRecord, TombstoneRecord
   - TypeDefRecord, RoleDefRecord, PropertyDefRecord
   - The `Value` tagged union (Section 11.2)
4. Implement UUID v7 generation (Section 8.1) and identifier helpers.
5. Implement the append-only log writer (Section 9.1).
6. Implement basic SSTable structure with the file layout from Section 11.5 (.ndb / .ndblog / MANIFEST / CURRENT / LOCK).
7. Commit incrementally on `feature/storage-core` off `develop`.

**Stack:** Rust edition 2024. Crates: `uuid` (with v7 feature), `crc32fast`, `zstd` or `lz4_flex`, `bytes`, `memmap2`. `cargo nextest` for tests. TDD on pure-function code (encoding, parsing, CRC, MVCC visibility).

**Architectural constraints (locked, do NOT re-litigate):**
- Append-only LSM storage
- MVCC with snapshot isolation default
- Single-writer + batching for v1
- Custom binary format on disk; JSON/JSONL only on the wire
- UUID v7 for internal IDs
- No "schema" or "namespace" primitives — metadata hyperedges instead
- `u64::MAX` sentinel for active supersession
- Dictionary encoding for type/role/property names
- Self-describing tagged-union values

If a genuine ambiguity surfaces, fix it inline in the spec and note the decision in the commit message. Don't reopen settled questions.

**Verification before completion:** run tests, show output, don't claim "done" without evidence.

### Remaining Acceptance Criteria

These are open items that don't block v1 implementation — they get decided in focused specs as we approach each phase:

- [ ] Vector index algorithm — HNSW vs IVF vs ScaNN (v2 spec, when the GPU vector plugin lands)
- [ ] Storage block size + alignment + crash recovery details (Section 11.4 sub-questions; decide during v1 implementation)
- [ ] Query language grammar formal spec — BNF/EBNF, operator precedence, subquery syntax (Section 12.9)
- [ ] Distribution mechanics — read replicas, federation (Section 14.1, v3+ scope)
- [ ] Error handling specifics (Section 14.4)
- [ ] Testing strategy specifics (Section 14.5)

### Learnings worth keeping

This session was creative architectural design, not bug-capture. The "learnings" are encoded in the two design documents themselves — section by section, with rationale, with rejected alternatives, with concrete examples across domains. There are no cross-project rules worth promoting separately beyond what's in the docs.

Notable design patterns the session validated (already implicitly in `programming.md` / `claude-process.md` rules):

- When porting concepts from one paradigm (SQL) to another (graph), question whether each inherited concept (schema, namespace, etc.) carries weight in the new paradigm — most don't.
- User pushback on the form "do we really need X?" usually identifies real complexity that can be removed.
- Engine minimalism + companion crates beats monolithic engines for new database designs (consistent with DuckDB/Polars/DataFusion industry trend).
- Separating an authoritative design spec (byte-level depth) from a public-facing white paper (narrative distillation) serves different audiences cleanly.

These are reinforcements of existing rules, not new ones. No promotion needed.
