//! Query planner + executor — wire `QueryRequest` → `QueryResponse`.
//!
//! Authoritative specs:
//! `docs/superpowers/specs/2026-05-27-query-language.md` (§5 semantics);
//! `docs/superpowers/specs/2026-05-27-v2-working-spec.md` (§2.6 planner).
//!
//! v2 implementation:
//!
//! - **Cardinality-aware planner.** [`plan::plan`] estimates each atom's
//!   cardinality from existing indexes (B-tree, adjacency, type cluster)
//!   and walks greedily from the smallest seed. Tiebreak on subsequent
//!   atoms = max shared variables with the bound set. See `plan.rs`.
//! - **Executor materialises bindings.** Each pattern transforms a
//!   `Vec<Bindings>` (current partial assignments) to a new `Vec<Bindings>`
//!   (extended assignments). Result set is materialised in memory; the
//!   streaming variant lives in `Engine::snapshot_iter_streaming` and is
//!   used by `/query_stream` for IO, not by the executor's join machinery.
//! - **Recursive patterns** use BFS with a visited set and a depth cap;
//!   see `execute_recursive_hyperedge` + §5.3 of the language spec.
//! - **`as_of`**: both `tx_id` and `timestamp_us` forms are honoured;
//!   missing timestamps raise `TimestampUnavailable`.
//!
//! Bindings are stored as the engine's native `Value` (not `JsonValue`)
//! to avoid round-tripping through tag enums on the hot path. The wire
//! layer converts on output.

pub mod plan;

pub use plan::{ExplainEntry, Plan, plan as plan_query};

use std::collections::{HashMap, HashSet};

use crate::engine::{Engine, EngineError};
use crate::id::{EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId};
use crate::mvcc::Resolved;
use crate::record::Record;
use crate::value::Value;
use crate::wire::JsonValue;
use crate::wire_query::{
    AsOf, CmpOp, Expr, Pattern, PropertyFilter, QueryRequest, QueryResponse, Recursion,
    ReturnItem, RoleBinding, Term,
};

/// Per-row variable assignments. Keyed by variable name (without the `?`).
pub type Bindings = HashMap<String, Value>;

/// Errors raised by the query executor.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// Underlying engine read failed.
    #[error("engine: {0}")]
    Engine(#[from] EngineError),

    /// Recursive pattern uses a configuration the executor can't act on —
    /// e.g. no role bindings to identify the start/end endpoints, or
    /// `from` term not bound to a concrete entity.
    #[error("recursion_config_invalid: {reason}")]
    RecursionConfigInvalid {
        /// Human-readable explanation.
        reason: String,
    },

    /// Recursive walk hit `max_depth` while the frontier was still
    /// non-empty. Spec §5.3 — the executor never silent-truncates.
    #[error("recursion_depth_exceeded: depth={depth} frontier_size={frontier_size}")]
    RecursionDepthExceeded {
        /// Depth at which the cap was hit.
        depth: u32,
        /// Number of unexpanded entities still on the frontier.
        frontier_size: usize,
    },

    /// `as_of` with a timestamp the engine has never seen committed in
    /// this session. v1 only tracks commit timestamps in memory; queries
    /// against timestamps before the current process started always hit
    /// this path.
    #[error("timestamp_unavailable: no tx_id committed at or before timestamp_us={timestamp_us}")]
    TimestampUnavailable {
        /// The requested timestamp.
        timestamp_us: i64,
    },

    /// `as_of` selected a tx that's been compacted out.
    #[error("snapshot_unavailable: tx_id={tx_id} no longer available")]
    SnapshotUnavailable {
        /// The `tx_id` that couldn't be served.
        tx_id: u64,
    },

    /// Pattern type id doesn't appear in the dictionary observation set —
    /// no rows can possibly match. The executor returns this as a hard
    /// error rather than silent empty so callers can distinguish "type
    /// has no records" from "type doesn't exist".
    #[error("type_not_indexed: type_id={type_id} has no live records")]
    TypeNotIndexed {
        /// The `type_id` with no observed records.
        type_id: u32,
    },

    /// Filter expression compares against an unbound variable. (Resolver
    /// should catch this; executor double-checks.)
    #[error("unbound_variable_at_exec: ?{name}")]
    UnboundVariableAtExec {
        /// The unbound variable.
        name: String,
    },
}

/// Top-level entry — plan and execute one query against the engine.
pub fn execute(engine: &mut Engine, req: QueryRequest) -> Result<QueryResponse, QueryError> {
    let snapshot = resolve_snapshot(engine, req.as_of)?;
    let mut rows: Vec<Bindings> = vec![Bindings::new()];

    // Cardinality-aware planner (v2.0): greedy smallest-seed,
    // shared-vars-first tiebreak. Result set is identical to source-order
    // execution — patterns commute under unification.
    let plan = plan::plan(engine, &req.patterns);
    for &idx in &plan.order {
        let pattern = &req.patterns[idx];
        rows = execute_pattern(engine, snapshot, pattern, rows)?;
        if rows.is_empty() {
            break;
        }
    }

    if let Some(ref expr) = req.filter {
        let mut kept = Vec::with_capacity(rows.len());
        for r in rows {
            if eval_filter(expr, &r)? {
                kept.push(r);
            }
        }
        rows = kept;
    }

    let mut truncated = false;
    if let Some(n) = req.limit
        && rows.len() > n
    {
        rows.truncate(n);
        truncated = true;
    }

    // Build the column header list (independent of row iteration so we
    // don't have to recompute per row).
    let columns: Vec<String> = req.returns.iter().map(ReturnItem::column_name).collect();

    let response_rows: Vec<Vec<JsonValue>> = rows
        .into_iter()
        .map(|r| {
            req.returns.iter()
                .map(|item| project_item(engine, snapshot, item, &r))
                .collect()
        })
        .collect();

    Ok(QueryResponse {
        columns,
        rows: response_rows,
        truncated,
    })
}

/// Project one `ReturnItem` for one row of bindings.
///
/// For `Variable`, the bound value is returned directly. For `Path`,
/// the bound value MUST be a UUID — we resolve it to the underlying
/// entity or hyperedge record at the snapshot and look up the property
/// by id. Missing UUID, missing record, or missing property all map to
/// `JsonValue::Null` — we never raise; per spec §5.6 a NULL projection
/// is the well-defined "not present at this snapshot" outcome.
fn project_item(
    engine: &mut Engine,
    snapshot: TxId,
    item: &ReturnItem,
    bindings: &Bindings,
) -> JsonValue {
    match item {
        ReturnItem::Variable(name) => bindings
            .get(name)
            .map_or(JsonValue::Null, |v| (&v.clone()).into()),

        ReturnItem::Path { variable, property, .. } => {
            // Must be a UUID-typed binding. The executor stores self-bound
            // entity / hyperedge UUIDs as Value::EntityRef (the storage
            // layer represents both kinds with the same uuid::Uuid).
            let Some(Value::EntityRef(eid)) = bindings.get(variable) else {
                return JsonValue::Null;
            };
            let uuid: uuid::Uuid = eid.into_uuid();
            let Ok(Resolved::Live(record)) = engine.snapshot_read(&uuid, snapshot) else {
                return JsonValue::Null;
            };
            // Pull the property by id from whichever record kind matched.
            let props_iter: Box<dyn Iterator<Item = &(PropertyId, Value)>> = match &record {
                Record::Entity(e)    => Box::new(e.properties.iter()),
                Record::HyperEdge(h) => Box::new(h.properties.iter()),
                _ => return JsonValue::Null,
            };
            let target = PropertyId::new(*property);
            for (pid, v) in props_iter {
                if *pid == target {
                    return (&v.clone()).into();
                }
            }
            JsonValue::Null
        }
    }
}

fn resolve_snapshot(engine: &Engine, as_of: Option<AsOf>) -> Result<TxId, QueryError> {
    match as_of {
        Some(AsOf::TxId { tx_id }) => Ok(TxId::new(tx_id)),
        Some(AsOf::TimestampUs { timestamp_us }) => engine
            .tx_at_or_before(timestamp_us)
            .ok_or(QueryError::TimestampUnavailable { timestamp_us }),
        None => Ok(TxId::new(engine.manifest().last_tx_id)),
    }
}

// ---------------------------------------------------------------------------
// Pattern execution
// ---------------------------------------------------------------------------

fn execute_pattern(
    engine: &mut Engine,
    snapshot: TxId,
    pattern: &Pattern,
    rows: Vec<Bindings>,
) -> Result<Vec<Bindings>, QueryError> {
    match pattern {
        Pattern::Entity {
            type_id,
            self_var,
            property_filters,
        } => execute_entity_pattern(
            engine,
            snapshot,
            *type_id,
            self_var.as_deref(),
            property_filters,
            rows,
        ),
        Pattern::Hyperedge {
            type_id,
            self_var,
            role_bindings,
            property_filters,
            recursion,
        } => {
            if let Some(rec) = recursion {
                return execute_recursive_hyperedge(
                    engine,
                    snapshot,
                    *type_id,
                    role_bindings,
                    property_filters,
                    rec,
                    rows,
                );
            }
            execute_hyperedge_pattern(
                engine,
                snapshot,
                *type_id,
                self_var.as_deref(),
                role_bindings,
                property_filters,
                rows,
            )
        }
    }
}

fn execute_entity_pattern(
    engine: &mut Engine,
    snapshot: TxId,
    type_id: u32,
    self_var: Option<&str>,
    property_filters: &[PropertyFilter],
    rows: Vec<Bindings>,
) -> Result<Vec<Bindings>, QueryError> {
    let mut out = Vec::new();
    for row in rows {
        // If self_var is already bound to a UUID, the entity is known —
        // just snapshot_read and check type + filters.
        if let Some(sv) = self_var
            && let Some(Value::EntityRef(eid)) = row.get(sv)
        {
            let eid = *eid;
            if let Some(rec) =
                entity_at(engine, snapshot, eid.into_uuid())?
                && rec.type_id == TypeId::new(type_id)
                && let Some(extended) =
                    apply_entity_filters(&rec, property_filters, row.clone())
            {
                out.push(extended);
            }
            continue;
        }

        // Otherwise scan: snapshot_iter and pick entities of the right type.
        // (Property B-tree indexes used as a fast path when applicable.)
        let candidates = candidate_entities_for_pattern(
            engine, snapshot, type_id, property_filters, &row,
        )?;
        for eid in candidates {
            let Some(rec) = entity_at(engine, snapshot, eid.into_uuid())? else {
                continue;
            };
            if rec.type_id != TypeId::new(type_id) {
                continue;
            }
            let mut row = row.clone();
            if let Some(sv) = self_var
                && !unify(&mut row, sv, Value::EntityRef(rec.entity_id))
            {
                continue;
            }
            if let Some(extended) = apply_entity_filters(&rec, property_filters, row) {
                out.push(extended);
            }
        }
    }
    Ok(out)
}

fn execute_hyperedge_pattern(
    engine: &mut Engine,
    snapshot: TxId,
    type_id: u32,
    self_var: Option<&str>,
    role_bindings: &[RoleBinding],
    property_filters: &[PropertyFilter],
    rows: Vec<Bindings>,
) -> Result<Vec<Bindings>, QueryError> {
    let mut out = Vec::new();
    for row in rows {
        // Candidate hyperedges — narrowed via adjacency when any role term
        // resolves to a concrete entity (bound var or literal UUID), else
        // via type cluster.
        let candidates = candidate_hyperedges(engine, type_id, role_bindings, &row);
        for hid in candidates {
            let Some(rec) = hyperedge_at(engine, snapshot, hid.into_uuid())? else {
                continue;
            };
            if rec.type_id != TypeId::new(type_id) {
                continue;
            }
            let mut row = row.clone();
            if let Some(sv) = self_var
                && !unify(
                    &mut row,
                    sv,
                    Value::EntityRef(EntityId::from_uuid(rec.hyperedge_id.into_uuid())),
                )
            {
                continue;
            }
            let Some(row) = apply_role_bindings(&rec.roles, role_bindings, row) else {
                continue;
            };
            let Some(row) = apply_hyperedge_property_filters(&rec.properties, property_filters, row)
            else {
                continue;
            };
            out.push(row);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Recursive hyperedge patterns — BFS with visited set + depth cap
// ---------------------------------------------------------------------------

/// Endpoints identified in a recursive pattern: exactly one role bound
/// to a concrete entity (start side) and exactly one role with a Var term
/// (end side). Other roles act as per-step constraints.
struct RecursiveEndpoints {
    /// Role id of the start side.
    from_role: u32,
    /// Role id of the end side.
    to_role: u32,
    /// Variable name to bind reachable endpoints.
    to_var: String,
    /// Constraints to apply at every step (non-endpoint role bindings).
    step_constraints: Vec<RoleBinding>,
}

fn identify_endpoints(
    role_bindings: &[RoleBinding],
    row: &Bindings,
) -> Result<(RecursiveEndpoints, EntityId), QueryError> {
    let mut from: Option<(u32, EntityId)> = None;
    let mut to: Option<(u32, String)> = None;
    let mut step = Vec::new();

    for rb in role_bindings {
        match &rb.term {
            // Bound or literal entity → from candidate.
            Term::Var { name } if row.get(name).is_some() => {
                if let Some(Value::EntityRef(eid)) = row.get(name) {
                    if from.is_some() {
                        step.push(rb.clone());
                    } else {
                        from = Some((rb.role_id, *eid));
                    }
                } else {
                    // Bound but not an entity ref — treat as constraint.
                    step.push(rb.clone());
                }
            }
            Term::Literal {
                value: JsonValue::Uuid { value },
            } => {
                let Ok(uuid) = uuid::Uuid::parse_str(value) else {
                    return Err(QueryError::RecursionConfigInvalid {
                        reason: format!("invalid uuid literal in role binding: {value}"),
                    });
                };
                if from.is_some() {
                    step.push(rb.clone());
                } else {
                    from = Some((rb.role_id, EntityId::from_uuid(uuid)));
                }
            }
            // Unbound variable → to candidate.
            Term::Var { name } => {
                if to.is_some() {
                    step.push(rb.clone());
                } else {
                    to = Some((rb.role_id, name.clone()));
                }
            }
            // Literal that's not a UUID can't be an endpoint of a walk.
            Term::Literal { .. } => step.push(rb.clone()),
        }
    }

    let (from_role, start_entity) = from.ok_or_else(|| QueryError::RecursionConfigInvalid {
        reason: "recursive pattern needs one role bound to a concrete entity (literal uuid or already-bound variable)".into(),
    })?;
    let (to_role, to_var) = to.ok_or_else(|| QueryError::RecursionConfigInvalid {
        reason: "recursive pattern needs one role bound to a fresh variable (the walk endpoint)".into(),
    })?;
    if from_role == to_role {
        return Err(QueryError::RecursionConfigInvalid {
            reason: "from and to roles must differ".into(),
        });
    }
    Ok((
        RecursiveEndpoints {
            from_role,
            to_role,
            to_var,
            step_constraints: step,
        },
        start_entity,
    ))
}

/// Decode the recursion modifier into `(min_steps, max_steps)`.
const fn recursion_bounds(rec: &Recursion) -> (u32, u32) {
    match *rec {
        Recursion::Star { max_depth } => (0, max_depth),
        Recursion::Plus { max_depth } => (1, max_depth),
        Recursion::Optional => (0, 1),
        Recursion::Bounded { min, max } => (min, max),
    }
}

fn execute_recursive_hyperedge(
    engine: &mut Engine,
    snapshot: TxId,
    type_id: u32,
    role_bindings: &[RoleBinding],
    property_filters: &[PropertyFilter],
    recursion: &Recursion,
    rows: Vec<Bindings>,
) -> Result<Vec<Bindings>, QueryError> {
    let (min_steps, max_steps) = recursion_bounds(recursion);
    let mut out = Vec::new();

    for row in rows {
        let (endpoints, start) = identify_endpoints(role_bindings, &row)?;

        let mut visited: HashSet<EntityId> = HashSet::new();
        visited.insert(start);
        let mut frontier: HashSet<EntityId> = HashSet::from([start]);

        // Track endpoints reachable at each step. Step 0 = the start
        // itself, which counts as a path of length 0 (only relevant for
        // Star and Bounded with min == 0).
        if min_steps == 0 {
            let mut new_row = row.clone();
            if unify(&mut new_row, &endpoints.to_var, Value::EntityRef(start)) {
                out.push(new_row);
            }
            // else: to_var pre-bound to a different value — skip without error.
        }

        let mut depth: u32 = 0;
        while !frontier.is_empty() && depth < max_steps {
            depth += 1;
            let mut next: HashSet<EntityId> = HashSet::new();
            for &current in &frontier {
                let incident = engine.hyperedges_for_entity(current);
                for hid in incident {
                    let Some(rec) = hyperedge_at(engine, snapshot, hid.into_uuid())? else {
                        continue;
                    };
                    if rec.type_id != TypeId::new(type_id) {
                        continue;
                    }
                    // Check that `current` is in the `from` role of this hyperedge.
                    let is_from = rec
                        .roles
                        .iter()
                        .any(|(rid, eid)| rid.get() == endpoints.from_role && *eid == current);
                    if !is_from {
                        continue;
                    }
                    // Apply per-step constraints (other named roles + property filters).
                    if !apply_step_constraints(
                        &rec.roles,
                        &rec.properties,
                        &endpoints.step_constraints,
                        property_filters,
                    ) {
                        continue;
                    }
                    // Find the entity playing the `to` role.
                    let Some((_, to_entity)) = rec
                        .roles
                        .iter()
                        .find(|(rid, _)| rid.get() == endpoints.to_role)
                    else {
                        continue;
                    };
                    let to_entity = *to_entity;
                    if visited.insert(to_entity) {
                        next.insert(to_entity);
                    }
                    // Emit a result row if this depth is within bounds.
                    if depth >= min_steps && depth <= max_steps {
                        let mut new_row = row.clone();
                        if unify(&mut new_row, &endpoints.to_var, Value::EntityRef(to_entity)) {
                            out.push(new_row);
                        }
                    }
                }
            }
            frontier = next;
        }

        // The depth cap is a safety net for open-ended recursion. For
        // `Bounded { min, max }` and `Optional` the user picked the cap
        // intentionally — hitting it is the expected terminus, not an
        // error. For `Star` and `Plus` the cap is `max_depth` (defaults
        // to 64) — hitting it with frontier still non-empty means the
        // user underestimated the depth, and we error loudly per spec
        // §5.3 rather than silently truncating.
        let is_safety_cap = matches!(recursion, Recursion::Star { .. } | Recursion::Plus { .. });
        if is_safety_cap && depth >= max_steps && !frontier.is_empty() {
            return Err(QueryError::RecursionDepthExceeded {
                depth,
                frontier_size: frontier.len(),
            });
        }
    }

    Ok(out)
}

fn apply_step_constraints(
    roles: &[(RoleId, EntityId)],
    properties: &[(PropertyId, Value)],
    role_constraints: &[RoleBinding],
    property_filters: &[PropertyFilter],
) -> bool {
    // Per-step role constraints — only literal-form matches (we don't
    // unify variables inside the walk; they would create per-step bindings
    // that v0 doesn't model).
    for rb in role_constraints {
        let actual = roles
            .iter()
            .find(|(rid, _)| rid.get() == rb.role_id)
            .map(|(_, e)| *e);
        let Some(actual) = actual else {
            return false;
        };
        match &rb.term {
            Term::Literal {
                value: JsonValue::Uuid { value },
            } => {
                let Ok(want) = uuid::Uuid::parse_str(value) else {
                    return false;
                };
                if actual.into_uuid() != want {
                    return false;
                }
            }
            _ => {
                // Unbound vars and non-uuid literals can't constrain a role.
                return false;
            }
        }
    }
    // Per-step property filters — literal-eq only at this layer.
    for f in property_filters {
        let actual = properties
            .iter()
            .find(|(pid, _)| pid.get() == f.property_id)
            .map(|(_, v)| v.clone());
        let Some(actual) = actual else {
            return false;
        };
        match &f.term {
            Term::Literal { value } => {
                let want = json_to_value(value);
                if !cmp_values(&actual, f.op, &want) {
                    return false;
                }
            }
            Term::Var { .. } => {
                // Vars inside per-step property filters are not supported in v0.
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Candidate-set selection — index-aware seed
// ---------------------------------------------------------------------------

fn candidate_entities_for_pattern(
    engine: &mut Engine,
    snapshot: TxId,
    type_id: u32,
    property_filters: &[PropertyFilter],
    row: &Bindings,
) -> Result<Vec<EntityId>, QueryError> {
    // Try to seed via a property B-tree lookup on the first filter whose
    // term is a literal AND the (type_id, property_id) pair is indexed.
    for f in property_filters {
        if let Term::Literal { value } = &f.term
            && f.op == CmpOp::Eq
        {
            let v = json_to_value(value);
            let candidates =
                engine.property_lookup(TypeId::new(type_id), PropertyId::new(f.property_id), &v);
            if !candidates.is_empty() {
                return Ok(candidates);
            }
        }
        // Filter against an already-bound variable on the LHS would let us
        // seed via property_lookup too — but v0 keeps this simple. Fall
        // through to the scan path.
        if let Term::Var { name } = &f.term
            && let Some(v) = row.get(name)
        {
            let candidates =
                engine.property_lookup(TypeId::new(type_id), PropertyId::new(f.property_id), v);
            if !candidates.is_empty() {
                return Ok(candidates);
            }
        }
    }

    // Fallback: scan all entities of this type. Sad path, but correct.
    // v2 will use the entity-by-type cluster index once it's built; for
    // now we walk snapshot_iter and filter.
    let snap = engine.snapshot_iter(snapshot)?;
    let mut out = Vec::new();
    for r in snap {
        if let Record::Entity(e) = r
            && e.type_id == TypeId::new(type_id)
        {
            out.push(e.entity_id);
        }
    }
    Ok(out)
}

fn candidate_hyperedges(
    engine: &Engine,
    type_id: u32,
    role_bindings: &[RoleBinding],
    row: &Bindings,
) -> Vec<HyperedgeId> {
    // If any role term is a concrete entity (bound var or literal UUID),
    // intersect with that entity's adjacency.
    for rb in role_bindings {
        if let Some(eid) = role_term_to_entity(&rb.term, row) {
            let adj = engine.hyperedges_for_entity(eid);
            if adj.is_empty() {
                return adj;
            }
            // Filter by type at the candidate stage to shrink the set;
            // the later snapshot_read still verifies.
            let type_match = engine.hyperedges_by_type(TypeId::new(type_id));
            let type_set: HashSet<HyperedgeId> = type_match.into_iter().collect();
            return adj.into_iter().filter(|h| type_set.contains(h)).collect();
        }
    }
    // Fallback: every hyperedge of this type.
    engine.hyperedges_by_type(TypeId::new(type_id))
}

fn role_term_to_entity(term: &Term, row: &Bindings) -> Option<EntityId> {
    match term {
        Term::Var { name } => match row.get(name)? {
            Value::EntityRef(eid) => Some(*eid),
            _ => None,
        },
        Term::Literal {
            value: JsonValue::Uuid { value },
        } => uuid::Uuid::parse_str(value).ok().map(EntityId::from_uuid),
        Term::Literal { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Filter application — runs after a candidate record is fetched
// ---------------------------------------------------------------------------

fn apply_entity_filters(
    rec: &crate::record::EntityRecord,
    property_filters: &[PropertyFilter],
    mut row: Bindings,
) -> Option<Bindings> {
    for f in property_filters {
        let prop_val = rec
            .properties
            .iter()
            .find(|(pid, _)| pid.get() == f.property_id)
            .map(|(_, v)| v.clone())?;
        if !match_filter(&prop_val, f, &mut row) {
            return None;
        }
    }
    Some(row)
}

fn apply_hyperedge_property_filters(
    properties: &[(PropertyId, Value)],
    property_filters: &[PropertyFilter],
    mut row: Bindings,
) -> Option<Bindings> {
    for f in property_filters {
        let prop_val = properties
            .iter()
            .find(|(pid, _)| pid.get() == f.property_id)
            .map(|(_, v)| v.clone())?;
        if !match_filter(&prop_val, f, &mut row) {
            return None;
        }
    }
    Some(row)
}

fn apply_role_bindings(
    roles: &[(RoleId, EntityId)],
    role_bindings: &[RoleBinding],
    mut row: Bindings,
) -> Option<Bindings> {
    for rb in role_bindings {
        let role_val = roles
            .iter()
            .find(|(rid, _)| rid.get() == rb.role_id)
            .map(|(_, e)| *e)?;
        match &rb.term {
            Term::Var { name } => {
                if !unify(&mut row, name, Value::EntityRef(role_val)) {
                    return None;
                }
            }
            Term::Literal {
                value: JsonValue::Uuid { value },
            } => {
                let want = uuid::Uuid::parse_str(value).ok()?;
                if role_val.into_uuid() != want {
                    return None;
                }
            }
            Term::Literal { .. } => return None, // role bound to non-UUID literal can't match
        }
    }
    Some(row)
}

fn match_filter(prop_val: &Value, f: &PropertyFilter, row: &mut Bindings) -> bool {
    match &f.term {
        Term::Var { name } => {
            // Bind-or-equality semantics: if the variable is already bound,
            // it must equal the property; else bind it.
            unify(row, name, prop_val.clone())
        }
        Term::Literal { value } => {
            let v = json_to_value(value);
            cmp_values(prop_val, f.op, &v)
        }
    }
}

fn unify(row: &mut Bindings, name: &str, v: Value) -> bool {
    if let Some(existing) = row.get(name) {
        existing == &v
    } else {
        row.insert(name.to_string(), v);
        true
    }
}

fn cmp_values(left: &Value, op: CmpOp, right: &Value) -> bool {
    use std::cmp::Ordering;
    let ord = match (left, right) {
        (Value::I64(a), Value::I64(b))
        | (Value::Timestamp(a), Value::Timestamp(b)) => Some(a.cmp(b)),
        (Value::F64(a), Value::F64(b)) => a.partial_cmp(b),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        _ if left == right => Some(Ordering::Equal),
        _ => None,
    };
    let Some(ord) = ord else {
        // Incomparable types → always false. Spec §5.5: "comparison is FALSE
        // for that candidate (does not crash)".
        return false;
    };
    matches!(
        (op, ord),
        (CmpOp::Eq, Ordering::Equal)
            | (CmpOp::Ne, Ordering::Less | Ordering::Greater)
            | (CmpOp::Lt, Ordering::Less)
            | (CmpOp::Le, Ordering::Less | Ordering::Equal)
            | (CmpOp::Gt, Ordering::Greater)
            | (CmpOp::Ge, Ordering::Greater | Ordering::Equal)
    )
}

// ---------------------------------------------------------------------------
// Where-clause evaluation
// ---------------------------------------------------------------------------

fn eval_filter(expr: &Expr, row: &Bindings) -> Result<bool, QueryError> {
    match expr {
        Expr::And { left, right } => Ok(eval_filter(left, row)? && eval_filter(right, row)?),
        Expr::Or { left, right } => Ok(eval_filter(left, row)? || eval_filter(right, row)?),
        Expr::Not { inner } => Ok(!eval_filter(inner, row)?),
        Expr::Cmp { left, op, right } => {
            let lv = term_value(left, row)?;
            let rv = term_value(right, row)?;
            Ok(cmp_values(&lv, *op, &rv))
        }
    }
}

fn term_value(t: &Term, row: &Bindings) -> Result<Value, QueryError> {
    match t {
        Term::Var { name } => row
            .get(name)
            .cloned()
            .ok_or_else(|| QueryError::UnboundVariableAtExec { name: name.clone() }),
        Term::Literal { value } => Ok(json_to_value(value)),
    }
}

// ---------------------------------------------------------------------------
// Snapshot reads — return the record only if visible AND of the right kind.
// ---------------------------------------------------------------------------

fn entity_at(
    engine: &mut Engine,
    snapshot: TxId,
    uuid: uuid::Uuid,
) -> Result<Option<crate::record::EntityRecord>, QueryError> {
    match engine.snapshot_read(&uuid, snapshot)? {
        Resolved::Live(Record::Entity(e)) => Ok(Some(e)),
        _ => Ok(None),
    }
}

fn hyperedge_at(
    engine: &mut Engine,
    snapshot: TxId,
    uuid: uuid::Uuid,
) -> Result<Option<crate::record::HyperEdgeRecord>, QueryError> {
    match engine.snapshot_read(&uuid, snapshot)? {
        Resolved::Live(Record::HyperEdge(h)) => Ok(Some(h)),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// JsonValue ↔ Value conversion (fallible; literals that can't represent
// in the internal Value enum become Null which never matches anything).
// ---------------------------------------------------------------------------

fn json_to_value(j: &JsonValue) -> Value {
    Value::try_from(j.clone()).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Tests — small in-process engine + canned data.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::too_many_lines)] // tests build canned schemas; flattening hurts readability
mod tests {
    use super::*;
    use crate::Engine;
    use crate::record::{EntityRecord, HyperEdgeRecord};
    use crate::value::Value;

    const T_CUSTOMER: u32 = 100;
    const T_SALES_ORDER: u32 = 200;
    const R_CUSTOMER: u32 = 10;
    const P_NAME: u32 = 30;
    const P_REGION: u32 = 31;
    const P_AMOUNT: u32 = 32;

    fn temp_engine(name: &str) -> Engine {
        let dir = std::env::temp_dir().join(format!("ndb-query-{name}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        Engine::create(&dir).unwrap()
    }

    fn seed_customer(engine: &mut Engine, name: &str, region: &str) -> EntityId {
        let eid = EntityId::now_v7();
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(T_CUSTOMER),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PropertyId::new(P_NAME), Value::String(name.into())),
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
            properties: vec![(PropertyId::new(P_AMOUNT), Value::I64(amount))],
        });
        txn.commit().unwrap();
        hid
    }

    fn entity_pattern_one_prop_filter() -> Pattern {
        Pattern::Entity {
            type_id: T_CUSTOMER,
            self_var: Some("c".into()),
            property_filters: vec![PropertyFilter {
                property_id: P_REGION,
                op: CmpOp::Eq,
                term: Term::Literal {
                    value: JsonValue::String {
                        value: "Vietnam".into(),
                    },
                },
            }],
        }
    }

    #[test]
    fn entity_pattern_returns_matching_uuids() {
        let mut engine = temp_engine("entity");
        let _alice = seed_customer(&mut engine, "Alice", "Vietnam");
        let _bob = seed_customer(&mut engine, "Bob", "Singapore");
        let _carol = seed_customer(&mut engine, "Carol", "Vietnam");

        let req = QueryRequest {
            as_of: None,
            patterns: vec![entity_pattern_one_prop_filter()],
            filter: None,
            returns: vec!["c".into()],
            limit: None,
        };
        let resp = execute(&mut engine, req).unwrap();
        assert_eq!(resp.columns, vec!["c"]);
        assert_eq!(resp.rows.len(), 2);
        assert!(!resp.truncated);
        for row in &resp.rows {
            assert!(matches!(row[0], JsonValue::Uuid { .. }));
        }
    }

    #[test]
    fn entity_pattern_binds_property_to_var() {
        let mut engine = temp_engine("entity-bind");
        seed_customer(&mut engine, "Alice", "Vietnam");
        seed_customer(&mut engine, "Bob", "Vietnam");

        let req = QueryRequest {
            as_of: None,
            patterns: vec![Pattern::Entity {
                type_id: T_CUSTOMER,
                self_var: Some("c".into()),
                property_filters: vec![PropertyFilter {
                    property_id: P_NAME,
                    op: CmpOp::Eq,
                    term: Term::Var { name: "n".into() },
                }],
            }],
            filter: None,
            returns: vec!["n".into()],
            limit: None,
        };
        let resp = execute(&mut engine, req).unwrap();
        assert_eq!(resp.rows.len(), 2);
        let names: HashSet<String> = resp
            .rows
            .iter()
            .filter_map(|r| match &r[0] {
                JsonValue::String { value } => Some(value.clone()),
                _ => None,
            })
            .collect();
        assert!(names.contains("Alice"));
        assert!(names.contains("Bob"));
    }

    #[test]
    fn hyperedge_pattern_joins_to_entity() {
        let mut engine = temp_engine("join");
        let alice = seed_customer(&mut engine, "Alice", "Vietnam");
        let bob = seed_customer(&mut engine, "Bob", "Singapore");
        let _so1 = seed_sales_order(&mut engine, alice, 5000);
        let _so2 = seed_sales_order(&mut engine, alice, 200);
        let _so3 = seed_sales_order(&mut engine, bob, 9000);

        // sales_order(customer: ?c, amount: ?a) customer(region: "Vietnam") as ?c
        let req = QueryRequest {
            as_of: None,
            patterns: vec![
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
                },
                Pattern::Entity {
                    type_id: T_CUSTOMER,
                    self_var: Some("c".into()),
                    property_filters: vec![PropertyFilter {
                        property_id: P_REGION,
                        op: CmpOp::Eq,
                        term: Term::Literal {
                            value: JsonValue::String {
                                value: "Vietnam".into(),
                            },
                        },
                    }],
                },
            ],
            filter: None,
            returns: vec!["a".into()],
            limit: None,
        };
        let resp = execute(&mut engine, req).unwrap();
        assert_eq!(resp.rows.len(), 2);
        let amounts: HashSet<i64> = resp
            .rows
            .iter()
            .filter_map(|r| match &r[0] {
                JsonValue::I64 { value } => Some(*value),
                _ => None,
            })
            .collect();
        assert_eq!(amounts, [5000, 200].into_iter().collect());
    }

    #[test]
    fn where_clause_filters_after_match() {
        let mut engine = temp_engine("where");
        let alice = seed_customer(&mut engine, "Alice", "Vietnam");
        let _ = seed_sales_order(&mut engine, alice, 5000);
        let _ = seed_sales_order(&mut engine, alice, 200);

        let req = QueryRequest {
            as_of: None,
            patterns: vec![Pattern::Hyperedge {
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
            }],
            filter: Some(Expr::Cmp {
                left: Term::Var { name: "a".into() },
                op: CmpOp::Gt,
                right: Term::Literal {
                    value: JsonValue::I64 { value: 1000 },
                },
            }),
            returns: vec!["a".into()],
            limit: None,
        };
        let resp = execute(&mut engine, req).unwrap();
        assert_eq!(resp.rows.len(), 1);
        assert!(matches!(resp.rows[0][0], JsonValue::I64 { value: 5000 }));
    }

    #[test]
    fn limit_caps_and_marks_truncated() {
        let mut engine = temp_engine("limit");
        for i in 0..5 {
            seed_customer(&mut engine, &format!("c{i}"), "Vietnam");
        }
        let req = QueryRequest {
            as_of: None,
            patterns: vec![entity_pattern_one_prop_filter()],
            filter: None,
            returns: vec!["c".into()],
            limit: Some(2),
        };
        let resp = execute(&mut engine, req).unwrap();
        assert_eq!(resp.rows.len(), 2);
        assert!(resp.truncated);
    }

    #[test]
    fn empty_match_returns_zero_rows() {
        let mut engine = temp_engine("empty");
        seed_customer(&mut engine, "Alice", "Singapore"); // no Vietnam customers
        let req = QueryRequest {
            as_of: None,
            patterns: vec![entity_pattern_one_prop_filter()],
            filter: None,
            returns: vec!["c".into()],
            limit: None,
        };
        let resp = execute(&mut engine, req).unwrap();
        assert_eq!(resp.rows.len(), 0);
        assert!(!resp.truncated);
    }

    #[test]
    fn recursion_without_endpoints_errors() {
        let mut engine = temp_engine("recursion-noep");
        let req = QueryRequest {
            as_of: None,
            patterns: vec![Pattern::Hyperedge {
                type_id: T_SALES_ORDER,
                self_var: None,
                role_bindings: vec![],
                property_filters: vec![],
                recursion: Some(crate::wire_query::Recursion::Star { max_depth: 64 }),
            }],
            filter: None,
            returns: vec![],
            limit: None,
        };
        let err = execute(&mut engine, req).unwrap_err();
        assert!(matches!(err, QueryError::RecursionConfigInvalid { .. }));
    }

    // -----------------------------------------------------------------
    // Recursion tests build a small containment graph and walk it.
    // -----------------------------------------------------------------

    const T_PROTEIN: u32 = 300;
    const T_CONTAINS: u32 = 400;
    const R_PARENT: u32 = 20;
    const R_CHILD: u32 = 21;

    fn seed_protein(engine: &mut Engine, name: &str) -> EntityId {
        let eid = EntityId::now_v7();
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(T_PROTEIN),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(P_NAME), Value::String(name.into()))],
        });
        txn.commit().unwrap();
        eid
    }
    fn seed_contains(engine: &mut Engine, parent: EntityId, child: EntityId) {
        let mut txn = engine.begin_write();
        txn.put_hyperedge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(T_CONTAINS),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![
                (RoleId::new(R_PARENT), parent),
                (RoleId::new(R_CHILD), child),
            ],
            properties: vec![],
        });
        txn.commit().unwrap();
    }

    fn star_pattern(start: EntityId, var: &str) -> Pattern {
        Pattern::Hyperedge {
            type_id: T_CONTAINS,
            self_var: None,
            role_bindings: vec![
                RoleBinding {
                    role_id: R_PARENT,
                    term: Term::Literal {
                        value: JsonValue::Uuid {
                            value: start.into_uuid().to_string(),
                        },
                    },
                },
                RoleBinding {
                    role_id: R_CHILD,
                    term: Term::Var {
                        name: var.to_string(),
                    },
                },
            ],
            property_filters: vec![],
            recursion: Some(crate::wire_query::Recursion::Star { max_depth: 16 }),
        }
    }

    #[test]
    fn recursion_star_includes_start_and_walks_all_descendants() {
        // body → organ → tissue → cell
        let mut engine = temp_engine("rec-star");
        let body = seed_protein(&mut engine, "body");
        let organ = seed_protein(&mut engine, "organ");
        let tissue = seed_protein(&mut engine, "tissue");
        let cell = seed_protein(&mut engine, "cell");
        seed_contains(&mut engine, body, organ);
        seed_contains(&mut engine, organ, tissue);
        seed_contains(&mut engine, tissue, cell);

        let req = QueryRequest {
            as_of: None,
            patterns: vec![star_pattern(body, "leaf")],
            filter: None,
            returns: vec!["leaf".into()],
            limit: None,
        };
        let resp = execute(&mut engine, req).unwrap();
        // 4 rows: body itself (0-step), organ, tissue, cell.
        assert_eq!(resp.rows.len(), 4);
        let uuids: HashSet<String> = resp
            .rows
            .iter()
            .filter_map(|r| match &r[0] {
                JsonValue::Uuid { value } => Some(value.clone()),
                _ => None,
            })
            .collect();
        for e in [body, organ, tissue, cell] {
            assert!(
                uuids.contains(&e.into_uuid().to_string()),
                "missing {e:?}"
            );
        }
    }

    #[test]
    fn recursion_plus_excludes_start() {
        let mut engine = temp_engine("rec-plus");
        let body = seed_protein(&mut engine, "body");
        let organ = seed_protein(&mut engine, "organ");
        let cell = seed_protein(&mut engine, "cell");
        seed_contains(&mut engine, body, organ);
        seed_contains(&mut engine, organ, cell);

        let mut pat = star_pattern(body, "leaf");
        if let Pattern::Hyperedge {
            ref mut recursion, ..
        } = pat
        {
            *recursion = Some(crate::wire_query::Recursion::Plus { max_depth: 16 });
        }
        let req = QueryRequest {
            as_of: None,
            patterns: vec![pat],
            filter: None,
            returns: vec!["leaf".into()],
            limit: None,
        };
        let resp = execute(&mut engine, req).unwrap();
        // 2 rows: organ, cell. body excluded.
        assert_eq!(resp.rows.len(), 2);
        let uuids: HashSet<String> = resp
            .rows
            .iter()
            .filter_map(|r| match &r[0] {
                JsonValue::Uuid { value } => Some(value.clone()),
                _ => None,
            })
            .collect();
        assert!(!uuids.contains(&body.into_uuid().to_string()));
        assert!(uuids.contains(&organ.into_uuid().to_string()));
        assert!(uuids.contains(&cell.into_uuid().to_string()));
    }

    #[test]
    fn recursion_bounded_filters_by_depth() {
        // body → organ → tissue → cell — want depth exactly 2 (tissue).
        let mut engine = temp_engine("rec-bounded");
        let body = seed_protein(&mut engine, "body");
        let organ = seed_protein(&mut engine, "organ");
        let tissue = seed_protein(&mut engine, "tissue");
        let cell = seed_protein(&mut engine, "cell");
        seed_contains(&mut engine, body, organ);
        seed_contains(&mut engine, organ, tissue);
        seed_contains(&mut engine, tissue, cell);

        let mut pat = star_pattern(body, "leaf");
        if let Pattern::Hyperedge {
            ref mut recursion, ..
        } = pat
        {
            *recursion = Some(crate::wire_query::Recursion::Bounded { min: 2, max: 2 });
        }
        let req = QueryRequest {
            as_of: None,
            patterns: vec![pat],
            filter: None,
            returns: vec!["leaf".into()],
            limit: None,
        };
        let resp = execute(&mut engine, req).unwrap();
        assert_eq!(resp.rows.len(), 1);
        match &resp.rows[0][0] {
            JsonValue::Uuid { value } => assert_eq!(*value, tissue.into_uuid().to_string()),
            other => panic!("expected uuid, got {other:?}"),
        }
    }

    #[test]
    fn recursion_handles_cycle_via_visited_set() {
        // Cyclic graph: a → b → c → a. Star walk must terminate.
        let mut engine = temp_engine("rec-cycle");
        let a = seed_protein(&mut engine, "a");
        let b = seed_protein(&mut engine, "b");
        let c = seed_protein(&mut engine, "c");
        seed_contains(&mut engine, a, b);
        seed_contains(&mut engine, b, c);
        seed_contains(&mut engine, c, a);

        let req = QueryRequest {
            as_of: None,
            patterns: vec![star_pattern(a, "leaf")],
            filter: None,
            returns: vec!["leaf".into()],
            limit: None,
        };
        let resp = execute(&mut engine, req).unwrap();
        // 3 unique entities: a (0-step), b, c.
        let uuids: HashSet<String> = resp
            .rows
            .iter()
            .filter_map(|r| match &r[0] {
                JsonValue::Uuid { value } => Some(value.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(uuids.len(), 3);
    }

    #[test]
    fn recursion_depth_cap_returns_loud_error() {
        // Linear chain longer than max_depth; require error not silent truncate.
        let mut engine = temp_engine("rec-cap");
        let nodes: Vec<EntityId> = (0..10)
            .map(|i| seed_protein(&mut engine, &format!("n{i}")))
            .collect();
        for i in 0..nodes.len() - 1 {
            seed_contains(&mut engine, nodes[i], nodes[i + 1]);
        }
        let mut pat = star_pattern(nodes[0], "leaf");
        if let Pattern::Hyperedge {
            ref mut recursion, ..
        } = pat
        {
            *recursion = Some(crate::wire_query::Recursion::Star { max_depth: 3 });
        }
        let req = QueryRequest {
            as_of: None,
            patterns: vec![pat],
            filter: None,
            returns: vec!["leaf".into()],
            limit: None,
        };
        let err = execute(&mut engine, req).unwrap_err();
        assert!(matches!(
            err,
            QueryError::RecursionDepthExceeded { depth: 3, .. }
        ));
    }

    #[test]
    fn as_of_timestamp_before_first_commit_errors() {
        let mut engine = temp_engine("ts-empty");
        let req = QueryRequest {
            as_of: Some(AsOf::TimestampUs {
                timestamp_us: 1, // far before any commit
            }),
            patterns: vec![entity_pattern_one_prop_filter()],
            filter: None,
            returns: vec!["c".into()],
            limit: None,
        };
        let err = execute(&mut engine, req).unwrap_err();
        assert!(matches!(err, QueryError::TimestampUnavailable { .. }));
    }

    #[test]
    fn as_of_timestamp_finds_matching_tx_in_session() {
        let mut engine = temp_engine("ts-session");
        seed_customer(&mut engine, "Alice", "Vietnam");
        // Record a known timestamp for the last committed tx so the
        // lookup is deterministic.
        let last_tx = TxId::new(engine.manifest().last_tx_id);
        engine.record_commit_timestamp(last_tx, 1_000_000);
        // Add another customer + record a later timestamp.
        seed_customer(&mut engine, "Bob", "Vietnam");
        let last_tx2 = TxId::new(engine.manifest().last_tx_id);
        engine.record_commit_timestamp(last_tx2, 2_000_000);

        // Query as_of timestamp_us = 1_500_000 → must see tx=last_tx
        // (only Alice committed by then).
        let req = QueryRequest {
            as_of: Some(AsOf::TimestampUs {
                timestamp_us: 1_500_000,
            }),
            patterns: vec![entity_pattern_one_prop_filter()],
            filter: None,
            returns: vec!["c".into()],
            limit: None,
        };
        let resp = execute(&mut engine, req).unwrap();
        assert_eq!(resp.rows.len(), 1, "only Alice should be visible at t=1.5s");
    }

    #[test]
    fn unify_returns_false_on_conflict() {
        let mut row: Bindings = HashMap::new();
        row.insert("x".into(), Value::I64(1));
        assert!(unify(&mut row, "x", Value::I64(1)));
        assert!(!unify(&mut row, "x", Value::I64(2)));
        assert!(unify(&mut row, "y", Value::I64(3)));
        assert_eq!(row.get("y"), Some(&Value::I64(3)));
    }

    #[test]
    fn cmp_values_basic_ordering() {
        assert!(cmp_values(&Value::I64(1), CmpOp::Lt, &Value::I64(2)));
        assert!(!cmp_values(&Value::I64(2), CmpOp::Lt, &Value::I64(2)));
        assert!(cmp_values(&Value::I64(2), CmpOp::Le, &Value::I64(2)));
        assert!(cmp_values(
            &Value::String("a".into()),
            CmpOp::Lt,
            &Value::String("b".into())
        ));
        // Cross-type compare via PartialEq fallback (Eq only).
        assert!(!cmp_values(
            &Value::I64(1),
            CmpOp::Eq,
            &Value::String("1".into())
        ));
    }
}
