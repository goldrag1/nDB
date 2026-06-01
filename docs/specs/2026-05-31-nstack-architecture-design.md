# nStack — Architecture & Phase-1 Kernel Design

*Working name: **nStack** (TBD). Date: 2026-05-31. Status: design, pending review.*

A compile-time-verified, n-dimensional application platform — and a ready-to-ship ERP built on it — designed so **AI coding agents build and test business modules with dramatically fewer bugs** than Frappe/Odoo allow.

---

## 1. Goal & non-goals

**Ultimate goal:** a ready-to-ship ERP system built on **Rust + nDB + React**, whose platform layers actively help an AI coding agent at **Level 1 (verification/safety)** and **Level 2 (best-practice conventions/reuse)** — in both **building and testing**.

**Non-goals (for now):**
- Not a general-purpose web framework. It is opinionated toward business/ERP applications.
- Not targeting a large human app-developer pool. Primary author = AI agents (+ a Rust-literate maintainer).
- Not competing on raw analytical scale in v1 (that is nDB's separate explorer track).

## 2. Thesis (why this, not Frappe/Odoo)

Two independent levers reduce AI-authored bugs:

1. **Verification strength** — an automatic oracle that rejects wrong code. Frappe/Odoo score ~0 here (Python, no compiler, runtime magic). A whole catalog of "compiles/runs but does the wrong thing" bugs survives to runtime.
2. **Bug-surface reduction** — code you never write. Frappe/Odoo score high here (DocType → table+form+API+perms).

Frappe/Odoo give lever 2 only. **nStack aims to give both**: lever 2 via conventions/codegen, lever 1 via the Rust compiler + a fast in-process test harness. The differentiator that hangs everything together:

> **A single typed, n-dimensional schema is the contract from which storage, logic-types, permissions, API, migrations, and all-form-factor UI are derived — and the compiler + auto-generated tests verify the whole graph end to end.**

Plus a third capability neither incumbent has: **native n-dimensional modeling** (nDB hyperedges = N-ary relations, MVCC time-travel = free audit/history).

## 3. Architecture (7 layers)

| # | Layer | Responsibility | Author |
|---|-------|----------------|--------|
| 1 | **nDB engine** | n-D store, MVCC time-travel, vector kNN, indexes | framework only |
| 2 | **Schema** | typed n-D contract (entities, dimensions, N-ary relations, invariants, lifecycle, permissions, views). Source of everything. | agent |
| 3 | **Logic** | Rust lifecycle/hook fns, typed nDB binding (no raw mutation), compiler-enforced | agent |
| 4 | **Verification + Test harness** | L1 (compiler + typed invariants) + L2 (auto property/scenario tests, typed fixtures, in-memory engine, fast `check`/`test` loop) | **agent's primary loop** |
| 5 | **Projection / UI** | typed view = projection of n-D model → universal React/React Native renderer, responsive; n-D explorer first-class | agent |
| 6 | **Runtime / platform** | multi-tenancy, auth, API, packaging/registry, auto-migrations, realtime change feed | ops |
| 7 | **ERP suite (flagship)** | accounting (VN VAS/TT99 + e-invoice), inventory, sales, purchase, manufacturing, HR/payroll — authored on 1–6, doubling as dogfood | the product |

### 3.1 How the schema maps onto nDB primitives (grounded in the engine API)

| nStack concept | nDB primitive | API reference |
|---|---|---|
| Entity (`#[entity] struct X`) | `EntityRecord { type_id, properties }` | `ndb_engine::record` |
| Field / dimension | `(PropertyId, Value)` | `record.rs` |
| `Ref<T>` (typed reference) | `Value::EntityRef(EntityId)` + commit-time existence check | `value.rs` |
| `Contains<Child>` / N-ary relation | `HyperEdgeRecord { roles, hyperedge_roles }` | `engine.put_hyperedge` |
| `Money<CUR>` | `Value::Decimal { scale, mantissa: i128 }` + compile-time currency tag | `value.rs` |
| `Lifecycle<S>` | property holding state + Rust typestate enforcement | framework |
| History / audit | MVCC `versions_of(id)`, `snapshot_read(id, as_of)` — **free** | `engine.rs` |
| Unique key / index | `register_lookup_key`, `register_property_btree` | `engine.rs` |
| Embedding / kNN | `Value::Vector`, `register_vector_property`, `vector_search` | `engine.rs` |

**Illustrative authoring surface** (Rust macro-DSL — final syntax TBD):

```rust
#[entity]
struct SalesOrder {
    customer: Ref<Customer>,              // -> Value::EntityRef + referential check
    lines:    Contains<SalesOrderLine>,   // -> hyperedge roles (parent/child)
    total:    Money<VND>,                 // -> Decimal; cross-currency math won't compile
    #[invariant(total == lines.sum(|l| l.amount))]
    state:    Lifecycle<SoState>,
}

#[states(Draft -> Confirmed -> Delivered, Draft -> Cancelled)]
enum SoState { Draft, Confirmed, Delivered, Cancelled }
// illegal transition (Delivered -> Draft) = compile error
```

Eliminated *at compile time*: wrong-currency arithmetic, unbalanced totals, illegal state jumps, dangling references — the exact bug classes that fill the Frappe rules catalog.

## 4. Layer 4 — Verification + Test harness (the 10x for agents)

The original problem: Frappe/Odoo verification loop is slow + stateful (bench → migrate → restart → browser → click) and has no compiler. nStack's loop:

```
agent edits schema/logic
  -> nstack check   (cargo check + schema-graph validation; seconds, local truthful errors)
  -> nstack test    (in-process; ephemeral nDB; ms)
  -> deterministic local failures -> agent self-corrects
```

**L1:** the Rust compiler + typed invariants. A module that compiles has cleared illegal-state, currency, and referential bug classes.

**L2 (framework-generated tests):**
- `#[invariant(...)]` → a property test (proptest) the framework runs automatically.
- `#[states(...)]` lifecycle → auto-generated transition-path tests (every legal path exercised; illegal paths asserted rejected).
- Schema → typed fixtures/factories (sample data generation per entity).
- **In-memory engine:** each test spins an ephemeral `Engine::create(temp_dir)` (confirmed feasible — `ndb-engine` is embeddable in-process), seeds typed fixtures, runs logic, asserts, cleans up. No bench, no server, no browser → millisecond loop.
- UI: typed views are snapshot-testable as a view-tree without a real browser; optional real-browser e2e.

This is the productized version of the Rust compiler loop, extended from L1 (types) to L2 (business-logic correctness).

## 5. nDB engine reality — gives / builds / constraints

**Engine GIVES (validated via API map):** in-process embeddable `Engine::create/open`, in-memory via temp dir, MVCC snapshot isolation + serializable mode, durable single-writer WAL transactions (`begin_write`→`put_*`→`commit`), n-ary hyperedges with roles, `Decimal`/`Vector` values, secondary indexes (lookup-key, property B-tree, vector), validation hooks (`require_property`, `expect_value_tag`), time-travel (`versions_of`, `snapshot_read(as_of)`). `ndb-studio` is the reference wrapper pattern.

**Framework must BUILD (this is layers 2–4's job):**
1. **Typed schema layer** — pin & persist `TypeId`/`PropertyId`/`RoleId` assignments (engine is schemaless; dictionary records exist but registrations are not auto-persisted). The kernel writes a **schema manifest** and replays registrations + constraints + index declarations on `open()`.
2. **Referential integrity** — verify `Ref`/hyperedge role-fillers exist at commit (engine does not).
3. **Derived/composite indexes** — engine indexes are single-key; framework builds composite/secondary structures where the schema declares them.
4. **Post-commit change feed** — engine has no CDC; framework adds a commit hook emitting change events (needed for realtime UI in layer 5).
5. **ORM-like typed binding** — domain structs ↔ records + queries (layer 3).

**Honest constraints to design around:**
- **Single-writer (v1).** Writes serialize globally (`SharedEngine` RwLock / `with_write_txn`). Acceptable for SMB ERP (Frappe effectively serializes hot rows too) but a **concurrency ceiling**; nDB v2 multi-writer is the path. *Design implication:* keep transactions short; treat write throughput as a known bound; revisit at v2.
- **Schema/constraint/index registrations + commit timestamps are session-local in v1.** Framework's schema-manifest replay handles registrations; as-of-**TxId** time-travel works now, as-of-**timestamp** is limited until nDB v2 persists `TxTimestampRecord`.
- **No batch-by-id read, no partial-text/composite index** — framework wraps/loops; full-text is out of scope v1.

**Phase-0 gate is NOT the explorer's 46s/10GB gate.** That is the *analytical* projection path. The ERP path is *transactional*: point lookups via `property_btree`/`lookup_key` + small `WriteTxn` commits — the path the engine is structurally built for. Phase 0 needs its own **transactional bench**: point-lookup p99, small-commit latency, concurrent-reader throughput under the single writer. This is a more achievable bar than the explorer gate.

## 6. Build roadmap

- **Phase 0 — nDB transactional gate.** Define + pass an OLTP bench (point-lookup p99, small-commit latency, concurrent readers). Prereq, but ERP-shaped (not the explorer gate).
- **Phase 1 — the Kernel.** Macro-DSL + typed nDB binding + schema-manifest persistence + Layer-4 verification/test harness + `check`/`test`/`migrate` CLI. *First sub-project — specced in §7.*
- **Phase 2 — UI projection.** Typed views → React renderer (desktop/laptop responsive first; React Native mobile next) + change feed for live updates.
- **Phase 3 — platform plane.** Auth, multi-tenant, API surface, packaging/registry.
- **Phase 4 — ERP suite.** Accounting → inventory → sales → … on the kernel. VN-first. "Ready-to-ship" materializes here; framework gets battle-tested.

## 7. Phase-1 Kernel — detailed spec (first sub-project)

**Outcome:** an AI agent can define one entity + its lifecycle logic in the macro-DSL, the framework stores it in nDB, and `check` + `test` verify it **in-process in seconds**, with a benchmark vs the equivalent Frappe implementation.

### 7.1 Components (each a focused crate/module)
- `nstack-schema` — the `#[entity]`, `#[states]`, `#[invariant]`, index/permission attribute macros. Emits: Rust types, a schema descriptor (type/property/role ids), and registration hooks.
- `nstack-engine-bind` — typed binding over `ndb-engine`: `insert/update/delete/get/query` in terms of domain structs; referential-integrity checks; schema-manifest write + replay-on-open.
- `nstack-test` — ephemeral in-memory engine harness, typed fixtures/factories, proptest generation from `#[invariant]`, transition-path test generation from `#[states]`.
- `nstack-cli` — `nstack check`, `nstack test`, `nstack migrate` (schema-diff → migration).

### 7.2 Data flow
schema macro → schema descriptor + Rust types → binding registers types/props/indexes on engine open (from manifest) → logic fns mutate only via binding (typed `WriteTxn`) → invariants checked at commit → tests run against ephemeral engine.

### 7.3 Error handling
- Compile-time: illegal states, currency mismatch, unknown refs → `cargo`/macro errors (local, truthful).
- Commit-time: invariant violation, referential miss → typed `KernelError` (no panics; rollback).
- Migration: schema-diff conflicts surfaced before apply.

### 7.4 Acceptance criteria (the proof of the load-bearing bet)
1. `#[entity] SalesOrder` + `SalesOrderLine` + `Confirm` transition + the `total == sum(lines)` invariant compile and persist to nDB.
2. `nstack test` runs the auto-generated invariant + transition tests against an in-memory engine; full loop **< a few seconds**, zero external services.
3. An attempt to (a) set an illegal currency, (b) make an illegal transition, or (c) leave totals unbalanced is **caught** — (a)/(b) at compile, (c) at commit — each with a clear local error.
4. **Benchmark vs Frappe:** implement the same Sales Order (confirm + balance rule + history) in the existing Frappe stack; measure **bug count, effort/LOC, iteration speed** of agent authoring. Kernel must win materially on all three, or the thesis is falsified and we stop.

### 7.5 Out of scope for Phase 1
UI, auth, multi-tenant, packaging, the ERP modules, multi-writer, full-text. Kernel only.

## 8. Open decisions (deferred, low-regret)
- Macro-DSL vs later schema-language facade → start macro-DSL; facade desugars to it.
- Permission model shape → defer to Phase 3.
- Change-feed transport (poll vs push) → defer to Phase 2.

## 9. Top risks (carried forward)
1. **The bet itself:** agents authoring Rust modules viably/cheaply. §7.4 is the explicit falsification test — run it early.
2. **Single-writer ceiling** for concurrent-user ERP (v1). Mitigate with short txns; resolve at nDB v2.
3. **Scope:** platform + ERP is a multi-phase, multi-month effort. Each phase ships an independently-useful artifact; the Kernel is first and proves the thesis.

## 10. Success metric (Phase 1)
Agent-authored Sales Order on nStack beats the Frappe equivalent on **bugs, effort, and loop speed** — with the entire correctness check running in-process in seconds. If yes → proceed to Phase 2. If no → the thesis is wrong and we have spent weeks, not years.
