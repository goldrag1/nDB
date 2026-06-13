# nStack — Session Handoff (2026-05-31)

## What this is
Building **nStack**: a compile-time-verified, n-dimensional ERP **platform + ready-to-ship ERP**, on **Rust + nDB + React**, designed so AI coding agents build/test business modules with far fewer bugs than Frappe/Odoo (L1 = compiler verification, L2 = conventions/reuse). User chose the maximally-ambitious path on every fork: **new platform play + Rust-all-the-way (agent-authored) + native n-dimensional**. User directive: "auto-continue until a complete ERP ready to ship" + "use subagents."

**Honest framing already given to user:** a complete ready-to-ship ERP is NOT achievable in one run (multi-year). Plan = grind verified, compiling, tested slices toward an **ERP nucleus** milestone, committing each checkpoint, never fake completion.

## Authoritative docs (read first)
- `docs/specs/2026-05-31-nstack-architecture-design.md` — full 7-layer architecture + Phase-1 Kernel spec + roadmap + §7.4 falsification test + honest nDB constraints. **This is the spec. Follow it.**
- Architecture layers: 1 nDB engine · 2 Schema (typed n-D, Rust macro-DSL) · 3 Logic (Rust hooks) · 4 **Verification+Test harness** (the 10x: compiler + auto-tests + in-memory engine + fast loop) · 5 Projection/UI (React+RN, one def→all screens) · 6 Runtime/platform · 7 ERP suite (VN-first, dogfood).
- Build order: Phase 0 nDB OLTP gate (NOT the 46s explorer gate — transactional path is different/easier) → Phase 1 Kernel → Phase 2 UI → Phase 3 platform → Phase 4 ERP modules.

## Current state — git
- Branch: **`feat/nstack`** (created off `feat/ndb-studio` HEAD). User's uncommitted ndb-studio WIP (`crates/ndb-studio/src/http.rs`, `store.rs`) stays unstaged in working tree — DO NOT commit those.
- **Slice 1 committed** (commit `9485f8e`, message "feat(nstack): Phase-1 kernel slice…"). ⚠️ Terminal output was garbled during verification — **first action next session: run `git -C /home/long/long/nDB-ndimemsion-database log --oneline -3` and `git status` to confirm clean state** before continuing.
- Commit message convention: end with `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. Only commit when verified green. Commit only nstack paths (`crates/nstack`, `docs/specs/2026-05-31-nstack-*`, `Cargo.toml`, `Cargo.lock`).

## What's built & GREEN (Slice 1)
`crates/nstack/` (added to root `Cargo.toml` members; `Cargo.toml` deps: `ndb-engine` path + `uuid` workspace). Single `src/lib.rs`, inline modules:
- `money` — `Money<C: Currency>` (VND scale0/USD scale2) → `Value::Decimal`. Cross-currency `+` is a **compile error** (compile_fail doctest proves it).
- `sales` — `SalesOrder<S>` typestate (Draft→Confirmed→Delivered/Cancelled). `.deliver()` only on Confirmed → illegal transitions are **compile errors** (compile_fail doctest). `confirm()` enforces `total == Σ lines` (commit-time invariant).
- `store::Store` — typed binding over embedded `ndb_engine::Engine`; `insert`/`get`.
- `customer::Customer` — Entity round-trip (hand-written to_record/from_record).
- `entity::Entity` trait; `testkit::TestDb` — ephemeral in-temp-dir engine, drop-cleanup.
- `error::KernelError`.
- **Tests: 4 unit + 2 compile_fail doctests pass.** `cargo test -p nstack`. Loop: check ~1.8s, test ~0.3s incremental.

## Key nDB engine API facts (from `crates/ndb-engine`, verified)
- Embeddable in-process: `Engine::create(path)` / `Engine::open(path)`. In-memory = temp dir. `examples/basic.rs` is the canonical usage template.
- `EntityRecord { entity_id, type_id, tx_id_assert: TxId::new(0), tx_id_supersede: TxId::ACTIVE, properties: Vec<(PropertyId, Value)> }`. `HyperEdgeRecord { roles: Vec<(RoleId,EntityId)>, hyperedge_roles, properties }` = N-ary relations.
- Write: `engine.begin_write()` → `txn.put_entity/put_hyperedge/delete` → `txn.commit()? -> TxId`. Read: `engine.snapshot_read(&id.into_uuid(), TxId::new(engine.manifest().last_tx_id))? -> Resolved::Live(Record::Entity(e))`. History: `versions_of(uuid)`.
- `Value`: Null/Bool/I64/F64/String/Bytes/Timestamp/EntityRef(EntityId)/**Decimal{scale:u8,mantissa:i128}**/Vector(Vec<f32>)/Extension. `PropertyId::new(u32)`/`.get()`, `TypeId::new`, `EntityId::now_v7()`/`.into_uuid()`.
- Indexes: `register_lookup_key`, `register_property_btree`, `register_vector_property` (+ lookups). Registrations + validation constraints + commit-timestamps are **session-local in v1** (re-register on open; framework must persist a schema manifest + replay).
- **CONSTRAINTS to design around:** v1 **single-writer** (SharedEngine serializes; short txns); no referential integrity (framework checks); no CDC (framework adds post-commit hook for realtime UI); no composite/full-text index. ndb-studio (`crates/ndb-studio/src/store.rs`) = reference wrapper pattern.
- Workspace: edition 2024, rustc 1.95, resolver 3. **Do NOT add `[lints]` to nstack Cargo.toml** (avoids inheriting workspace pedantic/missing_docs/unsafe-forbid). No network assumed — prefer no new external crates (use macro_rules, not syn/quote proc-macros, to avoid crates.io fetch).

## NEXT STEPS (in order)
**Slice 2 (was mid-design, NOT yet written): `entity!` macro_rules + `PropValue` trait** — kill the hand-written to_record/from_record boilerplate (proves agent-ergonomics half of thesis, zero new deps). Design ready:
- Add `pub mod prop` with `trait PropValue { fn to_value(&self)->Value; fn from_value(&Value)->Option<Self>; }` impls for `String`(Value::String), `i64`(I64), `EntityId`(EntityRef), `Money<C>`(Decimal{scale:C::SCALE,...} — inline, don't call Money::to_value to avoid name clash).
- Add `macro_rules! entity` at crate root BEFORE modules (textual scope). Syntax: `entity! { pub struct Customer = 1 { name: String = 11, email: String = 10, } }` → generates struct `#[derive(Debug,Clone,PartialEq)]` + `impl $crate::entity::Entity` with to_record (vec of `(PropertyId::new($pid), PropValue::to_value(&self.$field))`) and from_record (`let mut $field: Option<$ty> = None;` loop `match p.get() { $pid => $field = PropValue::from_value(v), .. }` then `Some(Self{ $field: $field? })`). Use absolute paths `::ndb_engine::...`, `$crate::...`.
- Replace `mod customer` with `mod catalog` defining Customer + Item via macro; update tests. Run `cargo test -p nstack`, commit.

**Then:** Slice 3 = persist stateful SalesOrder + lifecycle, read prior states via MVCC `versions_of` (the "free audit/history" claim). Slice 4 = **double-entry accounting nucleus** (JournalEntry with debits==credits invariant — best L1 showcase, ties to user's VAS/TT99 expertise). Slice 5+ = simple list/query, then more ERP modules.

**Critical reminder:** §7.4 of the spec = the go/no-go falsification test (agent-author one module on nStack vs the Frappe equivalent in `frappe-bench-das`, measure bugs/effort/speed). The whole bet is UNPROVEN until this runs. Recommend running it EARLY (after Slice 2/3) rather than building the entire ERP on an unvalidated premise — flag this to user.

## User working style (from CLAUDE.md/rules)
English in all artifacts (VN domain terms OK). Honest > cheerleading; report outcomes with evidence; never fake completion. Slot-machine: checkpoint (commit) before work, accept-or-revert. Hold position under stress-test. They chose max ambition deliberately and overruled the de-risk reframe — disagree-and-commit applies; serve the decision.
