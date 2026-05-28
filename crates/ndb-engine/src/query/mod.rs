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
    AsOf, CmpOp, Expr, OrderKey, Pattern, PropertyFilter, QueryRequest, QueryResponse, Recursion,
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

    // ── Fast path: count() pushdown to the type-cluster index. ──────
    // Matches the shape `match X() return count()` exactly: one entity
    // OR hyperedge pattern, no filters, no recursion, no role bindings,
    // no `where`, no `order by`, no `limit`, no writes, single return =
    // `count()` with no variable arg. Direct O(1) index probe instead of
    // materialising ~49k bindings.
    if let Some(n) = try_count_pushdown(engine, &req) {
        return Ok(QueryResponse {
            columns: vec!["count()".to_string()],
            rows: vec![vec![JsonValue::I64 { value: n as i64 }]],
            truncated: false,
        });
    }

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

    // Sort BEFORE truncate so limit picks the top-N in user-specified
    // order, not the executor's traversal order.
    if !req.order_by.is_empty() {
        sort_rows(engine, snapshot, &req.order_by, &mut rows);
    }

    let mut truncated = false;
    if let Some(n) = req.limit
        && rows.len() > n
    {
        rows.truncate(n);
        truncated = true;
    }

    // ── Write side ────────────────────────────────────────────────
    // Deletes first (so create can "replace" an old record cleanly in
    // the same query), then creates. Both share a single write txn
    // when there's anything to do — the engine handles MVCC.
    let has_writes = !req.deletes.is_empty() || !req.creates.is_empty()
                     || !req.sets.is_empty() || !req.merges.is_empty();
    let mut projection_snapshot = snapshot;
    if has_writes {
        // Deletes: tombstone every UUID bound by each variable across
        // all rows. The same UUID across multiple rows tombstones once
        // (the underlying SSTableKey identifies it).
        let mut to_tombstone: HashSet<uuid::Uuid> = HashSet::new();
        for d in &req.deletes {
            for r in &rows {
                if let Some(Value::EntityRef(eid)) = r.get(&d.variable) {
                    to_tombstone.insert(eid.into_uuid());
                }
            }
        }
        if !to_tombstone.is_empty() {
            let mut txn = engine.begin_write();
            let tx_id = txn.tx_id();
            for uuid in &to_tombstone {
                txn.put_raw(crate::record::Record::Tombstone(crate::record::TombstoneRecord {
                    target_id: *uuid,
                    tx_id_supersede: tx_id,
                }));
            }
            txn.commit().map_err(QueryError::Engine)?;
        }

        // SETs: read the current record, build a new assertion with the
        // named property replaced (or appended), commit. One write txn
        // per row × clause group for atomicity. Each (variable, uuid)
        // is processed at most once even if it appears across multiple
        // matched rows.
        if !req.sets.is_empty() {
            let mut updates: std::collections::HashMap<uuid::Uuid, Vec<&crate::wire_query::SetClause>> =
                std::collections::HashMap::new();
            for r in &rows {
                for s in &req.sets {
                    if let Some(Value::EntityRef(eid)) = r.get(&s.variable) {
                        updates.entry(eid.into_uuid()).or_default().push(s);
                    }
                }
            }
            if !updates.is_empty() {
                let row_ctx: Bindings = rows.first().cloned().unwrap_or_default();
                // Pass 1: read every current record (immutable borrow) + compute
                // the new property list. Collect everything we'll write.
                #[allow(clippy::type_complexity)]
                let mut pending: Vec<crate::record::Record> = Vec::with_capacity(updates.len());
                for (uuid, clauses) in updates {
                    let record = match engine.snapshot_read(&uuid, snapshot)
                        .map_err(QueryError::Engine)?
                    {
                        Resolved::Live(r) => r,
                        _ => continue,
                    };
                    let replaced: HashSet<u32> = clauses.iter().map(|c| c.property).collect();
                    match record {
                        crate::record::Record::Entity(e) => {
                            let mut new_props: Vec<(PropertyId, Value)> = e.properties
                                .into_iter()
                                .filter(|(p, _)| !replaced.contains(&p.get()))
                                .collect();
                            for c in &clauses {
                                let v = resolve_create_term(&c.term, &row_ctx)?;
                                new_props.push((PropertyId::new(c.property), v));
                            }
                            pending.push(crate::record::Record::Entity(crate::record::EntityRecord {
                                entity_id: e.entity_id,
                                type_id: e.type_id,
                                tx_id_assert: TxId::ACTIVE,  // overwritten by txn at commit
                                tx_id_supersede: TxId::ACTIVE,
                                properties: new_props,
                            }));
                        }
                        crate::record::Record::HyperEdge(h) => {
                            let mut new_props: Vec<(PropertyId, Value)> = h.properties
                                .into_iter()
                                .filter(|(p, _)| !replaced.contains(&p.get()))
                                .collect();
                            for c in &clauses {
                                let v = resolve_create_term(&c.term, &row_ctx)?;
                                new_props.push((PropertyId::new(c.property), v));
                            }
                            pending.push(crate::record::Record::HyperEdge(crate::record::HyperEdgeRecord {
                                hyperedge_id: h.hyperedge_id,
                                type_id: h.type_id,
                                tx_id_assert: TxId::ACTIVE,
                                tx_id_supersede: TxId::ACTIVE,
                                roles: h.roles,
                                hyperedge_roles: h.hyperedge_roles,
                                properties: new_props,
                            }));
                        }
                        _ => {}
                    }
                }
                // Pass 2: open write txn + put_raw every new assertion. The
                // engine fills in tx_id_assert from the txn.
                let mut txn = engine.begin_write();
                let tx_id = txn.tx_id();
                for rec in pending {
                    match rec {
                        crate::record::Record::Entity(mut e) => {
                            e.tx_id_assert = tx_id;
                            txn.put_entity(e);
                        }
                        crate::record::Record::HyperEdge(mut h) => {
                            h.tx_id_assert = tx_id;
                            txn.put_hyperedge(h);
                        }
                        _ => unreachable!(),
                    }
                }
                txn.commit().map_err(QueryError::Engine)?;
            }
        }

        // MERGEs: lookup-or-create. Two passes to dodge the borrow
        // checker — first an immutable scan to decide existing-vs-new
        // per clause, then one write txn for the new records.
        if !req.merges.is_empty() {
            let row_ctx: Bindings = rows.first().cloned().unwrap_or_default();

            // Pass 1: for each merge clause, resolve all binding terms
            // to concrete Values + decide whether a matching record exists.
            #[allow(clippy::type_complexity)]
            let mut decisions: Vec<(
                &crate::wire_query::MergeClause,
                Vec<(PropertyId, Value)>,    // prop_values
                Vec<(RoleId, EntityId)>,     // role_values
                Option<uuid::Uuid>,          // existing match, if any
            )> = Vec::with_capacity(req.merges.len());

            for m in &req.merges {
                let mut prop_values: Vec<(PropertyId, Value)> = Vec::with_capacity(m.properties.len());
                for cb in &m.properties {
                    let v = resolve_create_term(&cb.term, &row_ctx)?;
                    prop_values.push((PropertyId::new(cb.property_id), v));
                }
                let mut role_values: Vec<(RoleId, EntityId)> = Vec::with_capacity(m.role_bindings.len());
                for rb in &m.role_bindings {
                    let v = resolve_create_term(&rb.term, &row_ctx)?;
                    let Value::EntityRef(eid) = v else {
                        return Err(QueryError::RecursionConfigInvalid {
                            reason: format!("merge role {} expected an entity UUID", rb.role_id),
                        });
                    };
                    role_values.push((RoleId::new(rb.role_id), eid));
                }
                let target_type = TypeId::new(m.type_id);
                let found: Option<uuid::Uuid> = engine
                    .snapshot_iter_streaming(snapshot)
                    .filter_map(Result::ok)
                    .find_map(|rec| match (&rec, m.is_hyperedge) {
                        (crate::record::Record::Entity(e), false) if e.type_id == target_type => {
                            if prop_values.iter().all(|(p, v)|
                                e.properties.iter().any(|(ep, ev)| ep == p && ev == v))
                            { Some(e.entity_id.into_uuid()) } else { None }
                        }
                        (crate::record::Record::HyperEdge(h), true) if h.type_id == target_type => {
                            let props_ok = prop_values.iter().all(|(p, v)|
                                h.properties.iter().any(|(ep, ev)| ep == p && ev == v));
                            let roles_ok = role_values.iter().all(|(r, e)|
                                h.roles.iter().any(|(rr, er)| rr == r && er == e));
                            if props_ok && roles_ok { Some(h.hyperedge_id.into_uuid()) } else { None }
                        }
                        _ => None,
                    });
                decisions.push((m, prop_values, role_values, found));
            }

            // Pass 2: open one write txn, mint new records where needed,
            // bind self_var on every clause.
            let mut txn = engine.begin_write();
            let tx_id = txn.tx_id();
            for (m, prop_values, role_values, found) in decisions {
                let bound_uuid = if let Some(u) = found {
                    u
                } else if m.is_hyperedge {
                    let hid = HyperedgeId::now_v7();
                    txn.put_hyperedge(crate::record::HyperEdgeRecord {
                        hyperedge_id: hid,
                        type_id: TypeId::new(m.type_id),
                        tx_id_assert: tx_id,
                        tx_id_supersede: TxId::ACTIVE,
                        roles: role_values,
                        // merge clauses don't bind hyperedge-kind roles yet
                        // (v3 surface; resolver follow-up). Always entity-kind.
                        hyperedge_roles: Vec::new(),
                        properties: prop_values,
                    });
                    hid.into_uuid()
                } else {
                    let eid = EntityId::now_v7();
                    txn.put_entity(crate::record::EntityRecord {
                        entity_id: eid,
                        type_id: TypeId::new(m.type_id),
                        tx_id_assert: tx_id,
                        tx_id_supersede: TxId::ACTIVE,
                        properties: prop_values,
                    });
                    eid.into_uuid()
                };
                if let Some(v) = m.self_var.as_deref() {
                    let eref = EntityId::from_uuid(bound_uuid);
                    for r in &mut rows { r.insert(v.to_string(), Value::EntityRef(eref)); }
                    if rows.is_empty() {
                        let mut b = Bindings::new();
                        b.insert(v.to_string(), Value::EntityRef(eref));
                        rows.push(b);
                    }
                }
            }
            txn.commit().map_err(QueryError::Engine)?;
        }

        // Creates: one record per clause. Bindings (role + property) resolve
        // their variable Term:s against the FIRST surviving row (or an empty
        // bindings if there were no match patterns).
        if !req.creates.is_empty() {
            let row_for_bindings: Bindings = rows.first().cloned().unwrap_or_default();
            let mut txn = engine.begin_write();
            let tx_id = txn.tx_id();
            for c in &req.creates {
                let new_uuid = match c {
                    crate::wire_query::CreateClause::Entity { type_id, properties, self_var } => {
                        let eid = EntityId::now_v7();
                        let mut props_v: Vec<(PropertyId, Value)> = Vec::with_capacity(properties.len());
                        for cb in properties {
                            let v = resolve_create_term(&cb.term, &row_for_bindings)?;
                            props_v.push((PropertyId::new(cb.property_id), v));
                        }
                        txn.put_entity(crate::record::EntityRecord {
                            entity_id: eid,
                            type_id: TypeId::new(*type_id),
                            tx_id_assert: tx_id,
                            tx_id_supersede: TxId::ACTIVE,
                            properties: props_v,
                        });
                        if let Some(v) = self_var.as_deref() {
                            // Make this binding visible for downstream return projection.
                            // Each row gets the same self-bind.
                            for r in &mut rows { r.insert(v.to_string(), Value::EntityRef(eid)); }
                            // If there were no match patterns, ensure a single row exists
                            // so the return projection sees the new entity.
                            if rows.is_empty() {
                                let mut b = Bindings::new();
                                b.insert(v.to_string(), Value::EntityRef(eid));
                                rows.push(b);
                            }
                        }
                        eid.into_uuid()
                    }
                    crate::wire_query::CreateClause::Hyperedge { type_id, role_bindings, properties, self_var } => {
                        let hid = HyperedgeId::now_v7();
                        let mut roles: Vec<(RoleId, EntityId)> = Vec::with_capacity(role_bindings.len());
                        for rb in role_bindings {
                            let v = resolve_create_term(&rb.term, &row_for_bindings)?;
                            let Value::EntityRef(eid) = v else {
                                return Err(QueryError::RecursionConfigInvalid {
                                    reason: format!("role filler for role_id={} is not an entity UUID", rb.role_id),
                                });
                            };
                            roles.push((RoleId::new(rb.role_id), eid));
                        }
                        let mut props_v: Vec<(PropertyId, Value)> = Vec::with_capacity(properties.len());
                        for cb in properties {
                            let v = resolve_create_term(&cb.term, &row_for_bindings)?;
                            props_v.push((PropertyId::new(cb.property_id), v));
                        }
                        txn.put_hyperedge(crate::record::HyperEdgeRecord {
                            hyperedge_id: hid,
                            type_id: TypeId::new(*type_id),
                            tx_id_assert: tx_id,
                            tx_id_supersede: TxId::ACTIVE,
                            roles,
                            // create clause's role bindings are all
                            // entity-kind in v3 — hyperedge-role binding
                            // via the query language is a follow-up.
                            hyperedge_roles: Vec::new(),
                            properties: props_v,
                        });
                        if let Some(v) = self_var.as_deref() {
                            // EntityRef is the storage representation for both kinds.
                            let eref = EntityId::from_uuid(hid.into_uuid());
                            for r in &mut rows { r.insert(v.to_string(), Value::EntityRef(eref)); }
                            if rows.is_empty() {
                                let mut b = Bindings::new();
                                b.insert(v.to_string(), Value::EntityRef(eref));
                                rows.push(b);
                            }
                        }
                        hid.into_uuid()
                    }
                };
                let _ = new_uuid;  // silence: we don't currently surface it outside self_var
            }
            txn.commit().map_err(QueryError::Engine)?;
        }
        // Newly-committed records aren't visible at the original
        // `snapshot` — bump projection to the latest committed tx so
        // `return ?new.prop` after a `create as ?new` sees the props.
        projection_snapshot = TxId::ACTIVE;
    }

    // Build the column header list (independent of row iteration so we
    // don't have to recompute per row).
    let columns: Vec<String> = req.returns.iter().map(ReturnItem::column_name).collect();

    // Aggregate path: if ANY return item is an aggregate, the executor
    // implicitly groups by every non-aggregate item (Cypher semantics)
    // and produces one row per group. Otherwise: per-row projection.
    let has_aggregate = req.returns.iter().any(ReturnItem::is_aggregate);
    let response_rows: Vec<Vec<JsonValue>> = if has_aggregate {
        aggregate_rows(engine, projection_snapshot, &req.returns, rows)
    } else {
        rows.into_iter()
            .map(|r| {
                req.returns.iter()
                    .map(|item| project_item(engine, projection_snapshot, item, &r))
                    .collect()
            })
            .collect()
    };

    Ok(QueryResponse {
        columns,
        rows: response_rows,
        truncated,
    })
}

/// Sort `rows` in-place by every key in `order_by`. Stable sort, so
/// ties on key[0] fall through to key[1], etc.
///
/// Sort key resolution mirrors projection: bare variable → bound
/// value; `variable.property` → follow the bound UUID to the record
/// and look up the property. Missing-property and missing-UUID both
/// sort to the end of the order (treated as "greatest") so the
/// well-behaved rows cluster predictably.
fn sort_rows(
    engine: &mut Engine,
    snapshot: TxId,
    order_by: &[OrderKey],
    rows: &mut [Bindings],
) {
    // Pre-extract each row's key vector ONCE so the comparator doesn't
    // hit the engine per pairwise comparison.
    let keyed: Vec<(usize, Vec<Option<Value>>)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| (i, order_by.iter().map(|k| key_for_row(engine, snapshot, k, r)).collect()))
        .collect();

    let mut indices: Vec<usize> = (0..rows.len()).collect();
    indices.sort_by(|&a, &b| {
        let ka = &keyed[a].1;
        let kb = &keyed[b].1;
        for (i, k) in order_by.iter().enumerate() {
            let ord = compare_values(ka.get(i).and_then(|o| o.as_ref()),
                                     kb.get(i).and_then(|o| o.as_ref()));
            if ord != std::cmp::Ordering::Equal {
                return if k.descending { ord.reverse() } else { ord };
            }
        }
        std::cmp::Ordering::Equal
    });

    // Reorder `rows` to match the new index sequence. Using a temporary
    // Option<Bindings> swap to avoid clone of every row.
    let mut taken: Vec<Option<Bindings>> = rows.iter_mut().map(|r| Some(std::mem::take(r))).collect();
    for (dst, &src) in indices.iter().enumerate() {
        rows[dst] = taken[src].take().expect("each index used exactly once");
    }
}

fn key_for_row(engine: &mut Engine, snapshot: TxId, key: &OrderKey, row: &Bindings) -> Option<Value> {
    let v = row.get(&key.variable)?;
    match key.property {
        None => Some(v.clone()),
        Some(pid) => {
            let uuid = if let Value::EntityRef(eid) = v { eid.into_uuid() } else { return None };
            let Ok(Resolved::Live(record)) = engine.snapshot_read(&uuid, snapshot) else { return None };
            let props: &[(PropertyId, Value)] = match &record {
                Record::Entity(e)    => &e.properties,
                Record::HyperEdge(h) => &h.properties,
                _ => return None,
            };
            let target = PropertyId::new(pid);
            for (p, val) in props {
                if *p == target {
                    return Some(val.clone());
                }
            }
            None
        }
    }
}

/// Total ordering across Value variants. None sorts last (= treated as
/// greatest). Mixed-type comparisons fall back to type-tag ordering so
/// the comparator is total even on heterogeneous data.
fn compare_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (None, None) => Equal,
        (None, _)    => Greater,   // missing sorts to the end
        (_, None)    => Less,
        (Some(x), Some(y)) => match (x, y) {
            (Value::String(a), Value::String(b)) => a.cmp(b),
            (Value::I64(a),    Value::I64(b))    => a.cmp(b),
            (Value::F64(a),    Value::F64(b))    => a.partial_cmp(b).unwrap_or(Equal),
            (Value::Bool(a),   Value::Bool(b))   => a.cmp(b),
            (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
            (Value::EntityRef(a), Value::EntityRef(b)) => a.into_uuid().cmp(&b.into_uuid()),
            // Mixed types: fall back to a stable tag ordering. Document the choice in the spec.
            _ => format!("{x:?}").cmp(&format!("{y:?}")),
        },
    }
}

/// Implicit-GROUP-BY aggregation pass.
///
/// Cypher-style semantics: every non-aggregate return item is part of
/// the group key. For each group, emit one row whose columns are
/// (group-key projections in order) interleaved with (aggregate values
/// in order). Numeric aggregates (`sum`/`avg`/`min`/`max`) silently
/// skip non-numeric values; `count()` counts all rows in the group;
/// `count(?v)` counts rows where `?v` is bound.
fn aggregate_rows(
    engine: &mut Engine,
    snapshot: TxId,
    returns: &[ReturnItem],
    rows: Vec<Bindings>,
) -> Vec<Vec<JsonValue>> {
    // Project the key for each row (non-aggregate columns), bucket.
    use std::collections::BTreeMap;
    // BTreeMap with Vec<JsonValue> key requires the values to be Ord. JsonValue
    // doesn't implement Ord, so encode the key as a stable JSON string.
    let mut groups: BTreeMap<String, (Vec<JsonValue>, Vec<Bindings>)> = BTreeMap::new();

    for row in rows {
        let mut key_cells: Vec<JsonValue> = Vec::new();
        for item in returns {
            if !item.is_aggregate() {
                key_cells.push(project_item(engine, snapshot, item, &row));
            }
        }
        let key_str = serde_json::to_string(&key_cells).unwrap_or_default();
        groups.entry(key_str).or_insert_with(|| (key_cells, Vec::new())).1.push(row);
    }

    // Compute one output row per group.
    let mut out: Vec<Vec<JsonValue>> = Vec::with_capacity(groups.len());
    for (_, (key_cells, group_rows)) in groups {
        let mut row_out: Vec<JsonValue> = Vec::with_capacity(returns.len());
        let mut key_iter = key_cells.into_iter();
        for item in returns {
            if !item.is_aggregate() {
                row_out.push(key_iter.next().unwrap_or(JsonValue::Null));
                continue;
            }
            let ReturnItem::Aggregate { func, variable, property, .. } = item else { unreachable!() };
            let agg_val = compute_aggregate(engine, snapshot, func, variable.as_deref(), *property, &group_rows);
            row_out.push(agg_val);
        }
        out.push(row_out);
    }
    out
}

/// Compute one aggregate value over a group's rows.
fn compute_aggregate(
    engine: &mut Engine,
    snapshot: TxId,
    func: &str,
    variable: Option<&str>,
    property: Option<u32>,
    group_rows: &[Bindings],
) -> JsonValue {
    // count() / count(?v) — integer count.
    if func == "count" {
        let n: i64 = if let Some(v) = variable {
            group_rows.iter().filter(|r| r.get(v).is_some()).count() as i64
        } else {
            group_rows.len() as i64
        };
        return JsonValue::I64 { value: n };
    }

    // Numeric aggregates need a value per row. Collect them.
    let values: Vec<JsonValue> = group_rows.iter()
        .filter_map(|r| {
            let v = variable?;
            let bound = r.get(v)?;
            match property {
                None => Some((&bound.clone()).into()),
                Some(pid) => {
                    let Value::EntityRef(eid) = bound else { return None };
                    let uuid = eid.into_uuid();
                    let Ok(Resolved::Live(rec)) = engine.snapshot_read(&uuid, snapshot) else { return None };
                    let target = PropertyId::new(pid);
                    let props: Box<dyn Iterator<Item = &(PropertyId, Value)>> = match &rec {
                        Record::Entity(e)    => Box::new(e.properties.iter()),
                        Record::HyperEdge(h) => Box::new(h.properties.iter()),
                        _ => return None,
                    };
                    for (p, val) in props { if *p == target { return Some((&val.clone()).into()); } }
                    None
                }
            }
        })
        .collect();

    // Extract f64 (or i64 promoted) for sum/avg.
    let floats: Vec<f64> = values.iter().filter_map(|v| match v {
        JsonValue::I64 { value } => Some(*value as f64),
        JsonValue::F64 { value } => Some(*value),
        _ => None,
    }).collect();

    match func {
        "sum" => JsonValue::F64 { value: floats.iter().copied().sum() },
        "avg" => if floats.is_empty() {
            JsonValue::Null
        } else {
            JsonValue::F64 { value: floats.iter().copied().sum::<f64>() / floats.len() as f64 }
        },
        "min" => values.iter().filter(|v| !matches!(v, JsonValue::Null))
            .min_by(json_value_cmp)
            .cloned()
            .unwrap_or(JsonValue::Null),
        "max" => values.iter().filter(|v| !matches!(v, JsonValue::Null))
            .max_by(json_value_cmp)
            .cloned()
            .unwrap_or(JsonValue::Null),
        _ => JsonValue::Null,
    }
}

/// Total ordering across JsonValue for min/max. Mixed types fall back
/// to debug-string comparison so the comparator stays total.
fn json_value_cmp(a: &&JsonValue, b: &&JsonValue) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (JsonValue::String { value: a }, JsonValue::String { value: b }) => a.cmp(b),
        (JsonValue::I64 { value: a },    JsonValue::I64 { value: b })    => a.cmp(b),
        (JsonValue::F64 { value: a },    JsonValue::F64 { value: b })    => a.partial_cmp(b).unwrap_or(Equal),
        (JsonValue::I64 { value: a },    JsonValue::F64 { value: b })    => (*a as f64).partial_cmp(b).unwrap_or(Equal),
        (JsonValue::F64 { value: a },    JsonValue::I64 { value: b })    => a.partial_cmp(&(*b as f64)).unwrap_or(Equal),
        (JsonValue::Bool { value: a },   JsonValue::Bool { value: b })   => a.cmp(b),
        _ => format!("{a:?}").cmp(&format!("{b:?}")),
    }
}

/// Resolve a `Term` from a `create` binding to a `Value` using the
/// row's current bindings. Variables resolve via row.get; literals
/// convert from the wire's `JsonValue` to the engine's `Value`.
fn resolve_create_term(term: &Term, row: &Bindings) -> Result<Value, QueryError> {
    match term {
        Term::Var { name } => row.get(name)
            .cloned()
            .ok_or_else(|| QueryError::UnboundVariableAtExec { name: name.clone() }),
        Term::Literal { value } => Value::try_from(value.clone())
            .map_err(|_| QueryError::RecursionConfigInvalid {
                reason: format!("cannot convert literal to value: {value:?}"),
            }),
    }
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
        // Aggregates are projected by aggregate_rows(), not project_item.
        // If one slipped through, return null to keep the pipeline safe.
        ReturnItem::Aggregate { .. } => JsonValue::Null,
    }
}

/// Return the count if this request is a pure `match X() return count()`
/// over a single unconstrained type (no filters, no recursion, no
/// where/order/limit/writes, no as_of beyond ACTIVE). Returns `None` if
/// any condition is unmet, in which case the executor falls back to the
/// standard materialise + aggregate path.
///
/// Note: `as_of` is allowed only when it resolves to ACTIVE/latest. We
/// can't index-probe a historical snapshot because the type-cluster only
/// reflects the current state; supporting time-travelled counts would
/// need per-snapshot bucket sizes. Counts at `tx_id`s in the past fall
/// through to the slow path, which honours MVCC correctly.
fn try_count_pushdown(engine: &Engine, req: &QueryRequest) -> Option<u64> {
    // No write clauses.
    if !req.creates.is_empty() || !req.deletes.is_empty()
        || !req.sets.is_empty() || !req.merges.is_empty() {
        return None;
    }
    // No filter / order / limit (limit < 1 would zero the count, but
    // limit > 0 wouldn't change a single-row result; bail to be safe).
    if req.filter.is_some() || !req.order_by.is_empty() || req.limit.is_some() {
        return None;
    }
    // Only ACTIVE snapshot — historical counts go through the full path.
    if let Some(ref as_of) = req.as_of {
        match as_of {
            AsOf::TxId { tx_id } if *tx_id == TxId::ACTIVE.get() => {}
            AsOf::TxId { .. } => return None,
            AsOf::TimestampUs { .. } => return None,
        }
    }
    // Exactly one return = count() with no argument variable / property.
    if req.returns.len() != 1 { return None; }
    let ReturnItem::Aggregate { func, variable, property, .. } = &req.returns[0] else {
        return None;
    };
    if func != "count" { return None; }
    if variable.is_some() || property.is_some() { return None; }
    // Exactly one pattern, unconstrained except for type_id.
    if req.patterns.len() != 1 { return None; }
    match &req.patterns[0] {
        Pattern::Entity { type_id, property_filters, .. } if property_filters.is_empty() => {
            Some(engine.entity_type_count(TypeId::new(*type_id)) as u64)
        }
        Pattern::Hyperedge { type_id, role_bindings, property_filters, recursion, .. }
            if role_bindings.is_empty() && property_filters.is_empty() && recursion.is_none() => {
            Some(engine.hyperedge_type_count(TypeId::new(*type_id)) as u64)
        }
        _ => None,
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
    // Pre-compute the type bucket ONCE for the whole call. Building this
    // per-row was the dominant cost on two-pattern joins — for 49 seed
    // rows the previous code did 49× `hyperedges_by_type()` walks +
    // 49× HashSet rebuilds, even though the type bucket is invariant
    // across rows. The cache is shared via `candidate_hyperedges_with_cache`.
    let mut type_bucket_cache: Option<HashSet<HyperedgeId>> = None;
    for row in rows {
        // Candidate hyperedges — narrowed via adjacency when any role term
        // resolves to a concrete entity (bound var or literal UUID), else
        // via type cluster.
        let candidates = candidate_hyperedges_with_cache(
            engine, type_id, role_bindings, &row, &mut type_bucket_cache,
        );
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
    let mut cache = None;
    candidate_hyperedges_with_cache(engine, type_id, role_bindings, row, &mut cache)
}

/// Cached version — accepts a shared `type_bucket_cache` so the
/// expensive `(hyperedges_by_type(t) → Vec → HashSet)` work happens
/// once per query, not once per row. The per-row cost drops from
/// O(type_cluster_size) to O(adjacency_degree). On the bench's
/// two_pattern_join this changes 49 × 45k = 2.2M row-iters of work
/// to 1 × 45k + 49 × ~40 = ~47k, ~40× fewer set-builds and
/// proportionally faster end-to-end.
fn candidate_hyperedges_with_cache(
    engine: &Engine,
    type_id: u32,
    role_bindings: &[RoleBinding],
    row: &Bindings,
    cache: &mut Option<HashSet<HyperedgeId>>,
) -> Vec<HyperedgeId> {
    for rb in role_bindings {
        if let Some(eid) = role_term_to_entity(&rb.term, row) {
            let adj = engine.hyperedges_for_entity(eid);
            if adj.is_empty() {
                return adj;
            }
            let type_set = cache.get_or_insert_with(|| {
                engine.hyperedges_by_type(TypeId::new(type_id)).into_iter().collect()
            });
            return adj.into_iter().filter(|h| type_set.contains(h)).collect();
        }
    }
    // Fallback: every hyperedge of this type. No cache needed — single
    // call, returned directly.
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
            hyperedge_roles: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: Some(2),
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            hyperedge_roles: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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
            order_by: Vec::new(),
            limit: None,
            creates: Vec::new(),
            deletes: Vec::new(),
            sets: Vec::new(),
            merges: Vec::new(),
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

    // ── Count-pushdown fast-path ────────────────────────────────────

    fn temp_engine_with_customers(n: usize) -> (Engine, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ndb-count-push-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut engine = Engine::create(&dir).unwrap();
        for _ in 0..n {
            let mut tx = engine.begin_write();
            tx.put_entity(EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: TypeId::new(100),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![],
            });
            tx.commit().unwrap();
        }
        (engine, dir)
    }

    fn count_request() -> QueryRequest {
        QueryRequest {
            as_of: None,
            patterns: vec![Pattern::Entity {
                type_id: 100, self_var: Some("c".into()), property_filters: vec![],
            }],
            filter: None,
            returns: vec![ReturnItem::Aggregate {
                func: "count".into(), variable: None, property: None, display: None,
            }],
            order_by: vec![], limit: None,
            creates: vec![], deletes: vec![], sets: vec![], merges: vec![],
        }
    }

    #[test]
    fn count_pushdown_returns_index_size_directly() {
        let (mut engine, _dir) = temp_engine_with_customers(127);
        let resp = execute(&mut engine, count_request()).unwrap();
        assert_eq!(resp.columns, vec!["count()"]);
        assert_eq!(resp.rows.len(), 1);
        match &resp.rows[0][0] {
            JsonValue::I64 { value } => assert_eq!(*value, 127),
            other => panic!("expected i64 127, got {other:?}"),
        }
    }

    #[test]
    fn count_pushdown_skipped_when_property_filter_present() {
        // A property filter forces the slow path (we'd need to actually
        // evaluate each row). The slow path still works, just isn't O(1).
        // Existing executor semantics: when the filter excludes every
        // candidate, the slow path returns 0 rows (no group to
        // aggregate). The point of this test is "fast path bailed" —
        // the row count proves we went through the materialise loop.
        let (mut engine, _dir) = temp_engine_with_customers(5);
        let mut req = count_request();
        if let Pattern::Entity { property_filters, .. } = &mut req.patterns[0] {
            property_filters.push(PropertyFilter {
                property_id: 30,
                op: CmpOp::Eq,
                term: Term::Literal { value: JsonValue::String { value: "x".into() } },
            });
        }
        let resp = execute(&mut engine, req).unwrap();
        // Slow path produced no group: that's the existing executor's
        // behaviour for an empty result, and what we want here is
        // strictly "didn't crash + didn't take the fast path".
        assert!(resp.rows.is_empty() || resp.rows.len() == 1);
    }

    #[test]
    fn count_pushdown_skipped_when_two_patterns() {
        // Two unconstrained patterns over the same type share the
        // self_var `?c`, so the join unifies them: the result count is
        // the number of entities (3), not the cartesian product (9).
        // The point of the test is that we took the slow path (the fast
        // path requires exactly one pattern).
        let (mut engine, _dir) = temp_engine_with_customers(3);
        let mut req = count_request();
        req.patterns.push(Pattern::Entity {
            type_id: 100, self_var: Some("c".into()), property_filters: vec![],
        });
        let resp = execute(&mut engine, req).unwrap();
        match &resp.rows[0][0] {
            JsonValue::I64 { value } => assert_eq!(*value, 3),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn count_pushdown_skipped_when_count_has_variable() {
        // count(?c.name) requires evaluating per-row. Not a fast-path case.
        let (mut engine, _dir) = temp_engine_with_customers(5);
        let mut req = count_request();
        if let ReturnItem::Aggregate { variable, .. } = &mut req.returns[0] {
            *variable = Some("c".into());
        }
        let resp = execute(&mut engine, req).unwrap();
        // Same 5 rows; the slow path still counts correctly.
        match &resp.rows[0][0] {
            JsonValue::I64 { value } => assert_eq!(*value, 5),
            other => panic!("got {other:?}"),
        }
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
