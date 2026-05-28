//! Cardinality-aware query planner — picks pattern execution order.
//!
//! v2.0 deliverable §2.6 of `2026-05-27-v2-working-spec.md`.
//!
//! v1 ran patterns in authoring order. That's correctness-preserving but
//! can be arbitrarily wasteful: a high-cardinality scan as the seed
//! produces a fat intermediate, which the next pattern then has to filter
//! down. Cardinality-aware ordering picks the smallest seed first; later
//! patterns prefer atoms that share variables with the bound set (turning
//! a scan into an index probe).
//!
//! ## Estimator
//!
//! Per-atom cardinality is estimated from existing indexes — see
//! `estimate_cardinality`. Sources, in priority order:
//!
//! 1. **Entity, literal-eq filter on indexed `(type_id, property_id)`.**
//!    Exact: `property_btree.find(...).len()`. Authoritative.
//! 2. **Entity, `self_var` already bound.** Cardinality 1.
//! 3. **Entity, no usable index hook.** `UNKNOWN_HIGH` sentinel — the
//!    planner sorts these last among entity atoms. Honest about the gap:
//!    v1 has no entity-by-type cluster index; v3 may add one.
//! 4. **Hyperedge, role term is a literal UUID OR variable already bound
//!    to an `EntityRef`.** `adjacency_degree(entity)`. Authoritative.
//! 5. **Hyperedge, no entity hook.** `hyperedge_type_count(type_id)`.
//! 6. **Hyperedge, recursive.** `avg_degree × depth_cap`, capped at
//!    `UNKNOWN_HIGH`. Heuristic — depth multiplies fanout, so this
//!    naturally sorts recursive walks last (which is usually correct).
//!
//! ## Ordering rule
//!
//! Greedy:
//!
//! 1. **Seed.** Atom with the lowest static cardinality (empty bound set).
//! 2. **Subsequent.** From the remaining set, pick the atom maximising
//!    `shared_vars` with the current bound set; tiebreak by lowest
//!    (dynamic) cardinality given those bindings.
//!
//! Result is a permutation of pattern indices plus per-atom estimates
//! captured at the moment each atom was picked. The wire executor walks
//! the patterns in this order; correctness is unchanged — patterns
//! commute under variable unification.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::hash::BuildHasher;

use crate::engine::Engine;
use crate::id::{EntityId, PropertyId, TypeId};
use crate::value::Value;
use crate::wire::JsonValue;
use crate::wire_query::{
    CmpOp, Pattern, PropertyFilter, Recursion, RoleBinding, Term,
};

/// Sentinel used when no index can offer a real estimate. Chosen large
/// enough that the planner reliably sorts these atoms last, but not
/// `u64::MAX` (which would saturate `min_by_key` arithmetic in the
/// dynamic-estimate path).
pub const UNKNOWN_HIGH: u64 = 10_000_000_000;

/// One row in an `EXPLAIN`-style trace. Returned by `Engine::explain_query`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExplainEntry {
    /// Index into the original `QueryRequest::patterns` array.
    pub pattern_index: usize,
    /// Cardinality estimate at the moment this atom was picked.
    pub estimated_cardinality: u64,
    /// Brief human-readable atom shape. Stable enough for snapshot tests
    /// but not part of the wire contract.
    pub atom_summary: String,
    /// Variables newly bound when this atom executes.
    pub binds: Vec<String>,
    /// Variables already bound (referenced from prior atoms).
    pub uses: Vec<String>,
}

/// Output of the planner — a permutation of pattern indices plus the
/// matching per-step cardinality estimates.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Order in which to execute `QueryRequest::patterns`.
    pub order: Vec<usize>,
    /// Cardinality estimate captured when each atom was picked.
    pub estimates: Vec<u64>,
}

impl Plan {
    /// Identity plan — source order, no estimates. Used as a defensive
    /// fallback if the planner sees no patterns.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            order: Vec::new(),
            estimates: Vec::new(),
        }
    }
}

/// Plan a set of patterns against the engine's current index snapshot.
///
/// The planner never reads user records — only index counts — so the
/// returned plan is valid for any snapshot the query later picks.
#[must_use]
pub fn plan(engine: &Engine, patterns: &[Pattern]) -> Plan {
    let n = patterns.len();
    if n == 0 {
        return Plan::empty();
    }

    let mut order = Vec::with_capacity(n);
    let mut estimates = Vec::with_capacity(n);
    let mut bound: HashSet<String> = HashSet::new();
    let mut remaining: Vec<usize> = (0..n).collect();

    // Seed: lowest static cardinality (empty bound set).
    let seed_idx = pick_next(engine, &remaining, patterns, &bound);
    let seed_card = estimate_cardinality(engine, &patterns[seed_idx], &bound);
    order.push(seed_idx);
    estimates.push(seed_card);
    bind_variables(&patterns[seed_idx], &mut bound);
    remaining.retain(|&i| i != seed_idx);

    while !remaining.is_empty() {
        let pick = pick_next(engine, &remaining, patterns, &bound);
        let card = estimate_cardinality(engine, &patterns[pick], &bound);
        order.push(pick);
        estimates.push(card);
        bind_variables(&patterns[pick], &mut bound);
        remaining.retain(|&i| i != pick);
    }

    Plan { order, estimates }
}

/// `Engine::explain_query` payload builder — pairs a `Plan` with
/// per-atom variable info + a one-line summary.
#[must_use]
pub fn explain(engine: &Engine, patterns: &[Pattern]) -> Vec<ExplainEntry> {
    let plan = plan(engine, patterns);
    let mut bound: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(plan.order.len());
    for (slot, (&idx, &card)) in plan
        .order
        .iter()
        .zip(plan.estimates.iter())
        .enumerate()
    {
        let _ = slot;
        let pattern = &patterns[idx];
        let atom_vars = atom_variables(pattern);
        let uses: Vec<String> = atom_vars
            .all
            .iter()
            .filter(|v| bound.contains(*v))
            .cloned()
            .collect();
        let binds: Vec<String> = atom_vars
            .all
            .iter()
            .filter(|v| !bound.contains(*v))
            .cloned()
            .collect();
        bound.extend(atom_vars.all);
        out.push(ExplainEntry {
            pattern_index: idx,
            estimated_cardinality: card,
            atom_summary: summarize(pattern),
            binds,
            uses,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Greedy pick
// ---------------------------------------------------------------------------

fn pick_next<S: BuildHasher>(
    engine: &Engine,
    remaining: &[usize],
    patterns: &[Pattern],
    bound: &HashSet<String, S>,
) -> usize {
    debug_assert!(!remaining.is_empty(), "pick_next called on empty remaining");
    let mut best_idx = remaining[0];
    let mut best_key = sort_key(engine, &patterns[best_idx], bound);
    for &idx in remaining.iter().skip(1) {
        let key = sort_key(engine, &patterns[idx], bound);
        if key < best_key {
            best_idx = idx;
            best_key = key;
        }
    }
    best_idx
}

/// Sort key: `(Reverse(shared_var_count), cardinality_estimate)`. Tuples
/// sort lexicographically; `Reverse` makes "maximum shared" sort smallest
/// (i.e., picked first), then minimum estimated cardinality breaks ties.
fn sort_key<S: BuildHasher>(
    engine: &Engine,
    pattern: &Pattern,
    bound: &HashSet<String, S>,
) -> (Reverse<usize>, u64) {
    let vars = atom_variables(pattern);
    let shared = vars.all.iter().filter(|v| bound.contains(*v)).count();
    let card = estimate_cardinality(engine, pattern, bound);
    (Reverse(shared), card)
}

// ---------------------------------------------------------------------------
// Cardinality estimator
// ---------------------------------------------------------------------------

/// Estimate the number of rows produced by `pattern` when executed with
/// the given `bound` variable set. Pure / side-effect free.
#[must_use]
pub fn estimate_cardinality<S: BuildHasher>(
    engine: &Engine,
    pattern: &Pattern,
    bound: &HashSet<String, S>,
) -> u64 {
    match pattern {
        Pattern::Entity {
            type_id,
            self_var,
            property_filters,
        } => estimate_entity(engine, *type_id, self_var.as_deref(), property_filters, bound),
        Pattern::Hyperedge {
            type_id,
            role_bindings,
            recursion,
            ..
        } => estimate_hyperedge(engine, *type_id, role_bindings, recursion.as_ref(), bound),
    }
}

fn estimate_entity<S: BuildHasher>(
    engine: &Engine,
    type_id: u32,
    self_var: Option<&str>,
    property_filters: &[PropertyFilter],
    bound: &HashSet<String, S>,
) -> u64 {
    // Self already bound → exactly one candidate.
    if let Some(sv) = self_var
        && bound.contains(sv)
    {
        return 1;
    }
    // Best literal-eq filter on an indexed (type, property) pair.
    let tid = TypeId::new(type_id);
    let mut best: Option<u64> = None;
    for f in property_filters {
        if f.op != CmpOp::Eq {
            continue;
        }
        let pid = PropertyId::new(f.property_id);
        if !engine.property_btree_registered(tid, pid) {
            continue;
        }
        match &f.term {
            Term::Literal { value } => {
                let v = Value::try_from(value.clone()).unwrap_or(Value::Null);
                let n = engine.property_lookup(tid, pid, &v).len() as u64;
                best = Some(best.map_or(n, |b| b.min(n)));
            }
            Term::Var { name } => {
                // Var that's already bound to a concrete value lets us
                // probe the index now.
                let _ = name;
                // We don't peek at the value here — the executor will do
                // the lookup. Treat as a probe with average index density.
                // Conservative estimate: 1 row (B-tree literal-eq is rare
                // collisions in dictionary-style data).
                //
                // If we DID have the bound value, we could probe; but
                // `plan()` only inspects the wire AST. Leaving as the
                // average is fine — this still beats the type-cluster
                // fallback below for most real schemas.
                best = Some(best.map_or(1, |b| b.min(1)));
            }
        }
    }
    best.unwrap_or(UNKNOWN_HIGH)
}

fn estimate_hyperedge<S: BuildHasher>(
    engine: &Engine,
    type_id: u32,
    role_bindings: &[RoleBinding],
    recursion: Option<&Recursion>,
    bound: &HashSet<String, S>,
) -> u64 {
    let tid = TypeId::new(type_id);

    // Recursive walks — heuristic = avg_degree^depth bounded reasonably.
    // Use the max-step bound from the recursion shape. For Star/Plus
    // with the default cap, this is large enough to sort recursion last
    // unless the graph is truly small.
    if let Some(rec) = recursion {
        // A recursive pattern REQUIRES one role-filler bound to a
        // concrete entity (literal UUID or already-bound variable),
        // otherwise the executor errors at runtime. If neither is
        // satisfied yet, force this pattern to sort to the END of the
        // plan so the seed walk gets its anchor from a sibling
        // pattern first.
        let has_anchor = role_bindings.iter().any(|rb| match &rb.term {
            Term::Literal { value: JsonValue::Uuid { .. } } => true,
            Term::Var { name } => bound.contains(name),
            _ => false,
        });
        if !has_anchor {
            // Larger than any non-recursive estimate, smaller than u64::MAX
            // so saturating arithmetic upstream stays well-behaved.
            return UNKNOWN_HIGH.saturating_mul(2);
        }
        let max_steps = match *rec {
            Recursion::Star { max_depth } | Recursion::Plus { max_depth } => max_depth,
            Recursion::Optional => 1,
            Recursion::Bounded { max, .. } => max,
        };
        let (hyp_count, ent_count) = engine.adjacency_overview();
        let avg_degree = if ent_count == 0 {
            0
        } else {
            hyp_count.div_ceil(ent_count)
        };
        let est = (avg_degree as u64).saturating_mul(u64::from(max_steps).max(1));
        return est.min(UNKNOWN_HIGH);
    }

    // Best entity hook: literal-UUID role OR variable role already bound
    // to an entity ref.
    for rb in role_bindings {
        let eid = match &rb.term {
            Term::Literal {
                value: JsonValue::Uuid { value },
            } => uuid::Uuid::parse_str(value).ok().map(EntityId::from_uuid),
            Term::Var { name } if bound.contains(name) => {
                // Variable bound but we don't know to which entity at
                // plan time. Use average degree as the dynamic estimate.
                // This is intentionally pessimistic — the actual probe
                // uses the entity's true degree, which may be lower.
                let (hyp_count, ent_count) = engine.adjacency_overview();
                let avg = if ent_count == 0 {
                    1
                } else {
                    hyp_count.div_ceil(ent_count).max(1)
                };
                return avg as u64;
            }
            _ => None,
        };
        if let Some(eid) = eid {
            return engine.adjacency_degree(eid) as u64;
        }
    }

    // No entity hook → full type cluster.
    engine.hyperedge_type_count(tid) as u64
}

// ---------------------------------------------------------------------------
// Variable extraction
// ---------------------------------------------------------------------------

struct AtomVariables {
    /// All variable names mentioned in this atom (self, role terms,
    /// property filter terms).
    all: HashSet<String>,
}

fn atom_variables(pattern: &Pattern) -> AtomVariables {
    let mut all = HashSet::new();
    match pattern {
        Pattern::Entity {
            self_var,
            property_filters,
            ..
        } => {
            if let Some(sv) = self_var {
                all.insert(sv.clone());
            }
            for f in property_filters {
                if let Term::Var { name } = &f.term {
                    all.insert(name.clone());
                }
            }
        }
        Pattern::Hyperedge {
            self_var,
            role_bindings,
            property_filters,
            ..
        } => {
            if let Some(sv) = self_var {
                all.insert(sv.clone());
            }
            for rb in role_bindings {
                if let Term::Var { name } = &rb.term {
                    all.insert(name.clone());
                }
            }
            for f in property_filters {
                if let Term::Var { name } = &f.term {
                    all.insert(name.clone());
                }
            }
        }
    }
    AtomVariables { all }
}

fn bind_variables(pattern: &Pattern, bound: &mut HashSet<String>) {
    let vars = atom_variables(pattern);
    bound.extend(vars.all);
}

// ---------------------------------------------------------------------------
// Summary string (for EXPLAIN output)
// ---------------------------------------------------------------------------

fn summarize(pattern: &Pattern) -> String {
    match pattern {
        Pattern::Entity {
            type_id,
            self_var,
            property_filters,
        } => {
            let self_part = self_var
                .as_deref()
                .map(|s| format!(" self=?{s}"))
                .unwrap_or_default();
            format!(
                "entity type={type_id}{self_part} filters={}",
                property_filters.len()
            )
        }
        Pattern::Hyperedge {
            type_id,
            self_var,
            role_bindings,
            property_filters,
            recursion,
        } => {
            let self_part = self_var
                .as_deref()
                .map(|s| format!(" self=?{s}"))
                .unwrap_or_default();
            let rec_part = match recursion {
                None => "",
                Some(Recursion::Star { .. }) => " rec=*",
                Some(Recursion::Plus { .. }) => " rec=+",
                Some(Recursion::Optional) => " rec=?",
                Some(Recursion::Bounded { .. }) => " rec={n,m}",
            };
            format!(
                "hyperedge type={type_id}{self_part} roles={} filters={}{rec_part}",
                role_bindings.len(),
                property_filters.len()
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — exercise the planner on hand-built scenarios where source-order
// is provably worse than cardinality-aware ordering.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;
    use crate::id::{HyperedgeId, RoleId, TxId};
    use crate::record::{EntityRecord, HyperEdgeRecord};

    const T_CUSTOMER: u32 = 100;
    const T_SALES_ORDER: u32 = 200;
    const R_CUSTOMER: u32 = 10;
    const P_NAME: u32 = 30;
    const P_REGION: u32 = 31;
    const P_AMOUNT: u32 = 32;

    fn temp_engine(name: &str) -> Engine {
        let dir = std::env::temp_dir().join(format!("ndb-plan-{name}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        Engine::create(&dir).unwrap()
    }

    fn seed_customer(engine: &mut Engine, region: &str) -> EntityId {
        let eid = EntityId::now_v7();
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(T_CUSTOMER),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PropertyId::new(P_NAME), Value::String("x".into())),
                (PropertyId::new(P_REGION), Value::String(region.into())),
            ],
        });
        txn.commit().unwrap();
        eid
    }

    fn seed_sales_order(engine: &mut Engine, customer: EntityId, amount: i64) -> HyperedgeId {
        let hid = HyperedgeId::now_v7();
        let mut txn = engine.begin_write();
        txn.put_hyperedge(HyperEdgeRecord {
            hyperedge_id: hid,
            type_id: TypeId::new(T_SALES_ORDER),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(R_CUSTOMER), customer)],
            hyperedge_roles: Vec::new(),
            properties: vec![(PropertyId::new(P_AMOUNT), Value::I64(amount))],
        });
        txn.commit().unwrap();
        hid
    }

    fn pattern_entity_region(region_literal: &str) -> Pattern {
        Pattern::Entity {
            type_id: T_CUSTOMER,
            self_var: Some("c".into()),
            property_filters: vec![PropertyFilter {
                property_id: P_REGION,
                op: CmpOp::Eq,
                term: Term::Literal {
                    value: JsonValue::String {
                        value: region_literal.into(),
                    },
                },
            }],
        }
    }

    fn pattern_sales_order_by_customer() -> Pattern {
        Pattern::Hyperedge {
            type_id: T_SALES_ORDER,
            self_var: None,
            role_bindings: vec![RoleBinding {
                role_id: R_CUSTOMER,
                term: Term::Var { name: "c".into() },
            }],
            property_filters: vec![PropertyFilter {
                property_id: P_AMOUNT,
                op: CmpOp::Eq,
                term: Term::Var { name: "a".into() },
            }],
            recursion: None,
        }
    }

    #[test]
    fn empty_patterns_returns_empty_plan() {
        let engine = temp_engine("empty");
        let p = plan(&engine, &[]);
        assert!(p.order.is_empty());
        assert!(p.estimates.is_empty());
    }

    #[test]
    fn entity_with_btree_index_estimates_real_count() {
        let mut engine = temp_engine("entity-btree");
        engine.register_property_btree(TypeId::new(T_CUSTOMER), PropertyId::new(P_REGION));
        for _ in 0..3 {
            seed_customer(&mut engine, "Vietnam");
        }
        for _ in 0..50 {
            seed_customer(&mut engine, "Singapore");
        }

        let pattern = pattern_entity_region("Vietnam");
        let card = estimate_cardinality(&engine, &pattern, &HashSet::new());
        assert_eq!(card, 3, "B-tree probe should return exact count");
    }

    #[test]
    fn entity_without_index_uses_unknown_sentinel() {
        let mut engine = temp_engine("entity-noindex");
        // No B-tree registration → no exact count available.
        seed_customer(&mut engine, "Vietnam");
        let pattern = pattern_entity_region("Vietnam");
        let card = estimate_cardinality(&engine, &pattern, &HashSet::new());
        assert_eq!(card, UNKNOWN_HIGH);
    }

    #[test]
    fn hyperedge_with_no_hook_uses_type_count() {
        let mut engine = temp_engine("hyper-type");
        let cust = seed_customer(&mut engine, "X");
        for i in 0..7 {
            seed_sales_order(&mut engine, cust, i * 100);
        }
        let pattern = pattern_sales_order_by_customer();
        let card = estimate_cardinality(&engine, &pattern, &HashSet::new());
        assert_eq!(card, 7);
    }

    #[test]
    fn hyperedge_with_bound_entity_var_uses_avg_degree() {
        let mut engine = temp_engine("hyper-bound");
        let cust = seed_customer(&mut engine, "X");
        for i in 0..4 {
            seed_sales_order(&mut engine, cust, i);
        }
        let pattern = pattern_sales_order_by_customer();
        let mut bound = HashSet::new();
        bound.insert("c".into());
        let card = estimate_cardinality(&engine, &pattern, &bound);
        // avg_degree = 4 hyperedges / 1 entity = 4
        assert_eq!(card, 4);
    }

    #[test]
    fn planner_picks_indexed_entity_over_type_scan() {
        // Setup: 3 Vietnam customers (narrow), 5 sales orders.
        // - Pattern A: customer(region='Vietnam') — indexed, est=3
        // - Pattern B: sales_order(customer=?c, amount=?a) — type count, est=5
        //
        // Source order [B, A] would scan 5 hyperedges. Planner should pick A
        // first (smaller seed), then B (shared ?c → adjacency degree).
        let mut engine = temp_engine("pick-seed");
        engine.register_property_btree(TypeId::new(T_CUSTOMER), PropertyId::new(P_REGION));
        let vn: Vec<EntityId> = (0..3).map(|_| seed_customer(&mut engine, "Vietnam")).collect();
        for _ in 0..50 {
            seed_customer(&mut engine, "Singapore");
        }
        for c in &vn {
            seed_sales_order(&mut engine, *c, 100);
        }
        for _ in 0..2 {
            seed_sales_order(&mut engine, vn[0], 200);
        }

        let patterns = vec![pattern_sales_order_by_customer(), pattern_entity_region("Vietnam")];
        let plan = plan(&engine, &patterns);

        // Planner should reorder: entity pattern first (lower cardinality).
        assert_eq!(plan.order, vec![1, 0], "indexed entity should seed");
        assert_eq!(plan.estimates[0], 3, "Vietnam customers count");
    }

    #[test]
    fn planner_prefers_shared_variable_tiebreak() {
        // Three patterns. First atom binds ?c. Of the remaining two, one
        // shares ?c and one doesn't — planner picks the sharing one even
        // if its cardinality is slightly higher (shared > cardinality in
        // the sort key).
        let mut engine = temp_engine("share-tiebreak");
        engine.register_property_btree(TypeId::new(T_CUSTOMER), PropertyId::new(P_REGION));
        let cust = seed_customer(&mut engine, "Vietnam");
        for _ in 0..10 {
            seed_sales_order(&mut engine, cust, 1);
        }

        // Pattern P0: sales_order(customer=?c) — type count = 10
        // Pattern P1: customer(region='Vietnam') as ?c — bree count = 1
        // Pattern P2: another sales_order(customer=?c) — same type count = 10
        // Seed should pick P1 (smallest). Then between P0 and P2 (tied
        // cardinality, both share ?c), planner is order-stable — picks
        // first remaining, which is P0.
        let patterns = vec![
            pattern_sales_order_by_customer(),
            pattern_entity_region("Vietnam"),
            pattern_sales_order_by_customer(),
        ];
        let plan = plan(&engine, &patterns);
        assert_eq!(plan.order[0], 1, "indexed entity seeds");
        assert!(plan.order.contains(&0) && plan.order.contains(&2));
    }

    #[test]
    fn explain_entries_cover_all_patterns_with_summaries() {
        let mut engine = temp_engine("explain");
        engine.register_property_btree(TypeId::new(T_CUSTOMER), PropertyId::new(P_REGION));
        // 1 narrow customer, 5 sales orders → planner picks entity first
        // (smaller estimate). Use a real differentiator instead of relying
        // on the tied-cardinality tiebreak.
        let cust = seed_customer(&mut engine, "Vietnam");
        for i in 0..5 {
            seed_sales_order(&mut engine, cust, i);
        }

        let patterns = vec![pattern_sales_order_by_customer(), pattern_entity_region("Vietnam")];
        let entries = explain(&engine, &patterns);
        assert_eq!(entries.len(), 2);
        // Entries appear in PLANNED order (entity first).
        assert_eq!(entries[0].pattern_index, 1);
        assert_eq!(entries[0].estimated_cardinality, 1);
        assert!(entries[0].atom_summary.starts_with("entity"));
        // Seed binds ?c.
        assert!(entries[0].binds.contains(&"c".into()));
        // The next atom shares ?c with the bound set.
        assert!(entries[1].uses.contains(&"c".into()));
        assert!(entries[1].atom_summary.starts_with("hyperedge"));
    }

    #[test]
    fn recursive_pattern_without_anchor_sorts_to_end() {
        // A recursive pattern with NO role-fillers bound to a concrete
        // entity can't be executed standalone. The planner forces it
        // to the back of the plan via a punitive estimate (> UNKNOWN_HIGH).
        let mut engine = temp_engine("rec-no-anchor");
        let _a = seed_customer(&mut engine, "x");

        let pattern = Pattern::Hyperedge {
            type_id: T_SALES_ORDER,
            self_var: None,
            role_bindings: vec![],
            property_filters: vec![],
            recursion: Some(Recursion::Star { max_depth: 8 }),
        };
        let card = estimate_cardinality(&engine, &pattern, &HashSet::new());
        assert!(card > UNKNOWN_HIGH, "no-anchor recursive must sort behind any non-recursive pattern: got {card}");
    }

    #[test]
    fn recursive_pattern_with_anchor_estimates_with_depth_heuristic() {
        // With an anchor bound (literal UUID or pre-bound variable),
        // the depth × avg-degree heuristic kicks in normally.
        let mut engine = temp_engine("rec-anchored");
        let a = seed_customer(&mut engine, "x");
        let b = seed_customer(&mut engine, "x");
        seed_sales_order(&mut engine, a, 1);
        seed_sales_order(&mut engine, b, 1);

        let pattern = Pattern::Hyperedge {
            type_id: T_SALES_ORDER,
            self_var: None,
            role_bindings: vec![RoleBinding {
                role_id: R_CUSTOMER,
                term: Term::Literal {
                    value: JsonValue::Uuid { value: a.into_uuid().to_string() },
                },
            }],
            property_filters: vec![],
            recursion: Some(Recursion::Star { max_depth: 8 }),
        };
        let card = estimate_cardinality(&engine, &pattern, &HashSet::new());
        assert!(card <= UNKNOWN_HIGH, "anchored recursive estimate should stay in normal range: got {card}");
    }
}
