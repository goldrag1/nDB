//! Engine host + catalog: the single place that talks to `ndb-engine`.
//!
//! Everything above this (the HTTP layer) deals only in `serde_json::Value`.
//! `Store` owns the `SharedEngine`, derives the catalog by scanning, projects
//! record-kinds into tables, and turns create/edit/delete into MVCC commits.
//!
//! Names: nDB stores human names for types and properties as `TypeName` /
//! `PropertyKey` records in the same log. `Store` reads them to label the
//! catalog, and *interns* new ones (allocate the next free id + write the name
//! record) when a create/edit references a kind or property that does not yet
//! exist — so a freshly `--new`'d database is usable from an empty start.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use ndb_engine::engine::EngineError;
use ndb_engine::id::{EntityId, PropertyId, TxId, TypeId};
use ndb_engine::mvcc::Resolved;
use ndb_engine::record::{EntityRecord, PropertyKeyRecord, Record, TypeNameRecord};
use ndb_engine::shared::SharedEngine;
use ndb_engine::value::Value;
use serde_json::{Value as J, json};
use uuid::Uuid;

use crate::jsonval::{to_json, type_hint};

/// Owns the engine and serves every data operation the UI needs.
pub struct Store {
    engine: SharedEngine,
}

/// Errors a write path can surface, each mapping to an HTTP status.
#[derive(Debug)]
pub enum StoreError {
    /// An underlying engine error (includes `WriteStalled` backpressure).
    Engine(EngineError),
    /// The targeted record does not exist at the current snapshot.
    NotFound,
    /// The request carried a value v1 cannot store.
    BadValue(String),
}

impl From<EngineError> for StoreError {
    fn from(e: EngineError) -> Self {
        Self::Engine(e)
    }
}

impl StoreError {
    /// HTTP status this error maps to.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::Engine(EngineError::WriteStalled { .. }) => 503,
            Self::NotFound => 404,
            Self::BadValue(_) => 400,
            Self::Engine(_) => 500,
        }
    }

    /// A short machine code for the error envelope.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Engine(EngineError::WriteStalled { .. }) => "write_stalled",
            Self::NotFound => "not_found",
            Self::BadValue(_) => "bad_value",
            Self::Engine(_) => "engine_error",
        }
    }

    /// A human-readable message.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Engine(e) => format!("{e}"),
            Self::NotFound => "record not found".to_string(),
            Self::BadValue(m) => m.clone(),
        }
    }
}

/// Reserved kind for user accounts (hidden from data views).
const USER_KIND: &str = "$User";
/// Reserved property recording who wrote each version.
const AUTHOR_PROP: &str = "$author";

/// Reserved names (kinds and properties) begin with `$` and are filtered out
/// of every data-facing view — they hold the app's own metadata (users,
/// author attribution), not user data.
fn is_reserved(name: &str) -> bool {
    name.starts_with('$')
}

/// The value of property `pid` on an entity, if present.
fn prop_val(e: &EntityRecord, pid: u32) -> Option<&Value> {
    e.properties.iter().find(|(p, _)| p.get() == pid).map(|(_, v)| v)
}

/// The owned string of a `Value::String`, else `None`.
fn str_of(v: &Value) -> Option<String> {
    if let Value::String(s) = v { Some(s.clone()) } else { None }
}

/// A display-string grouping key for a pivot axis. Missing/null share `∅`,
/// scalars use their natural rendering, rich variants their tag.
fn cell_key(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "∅".to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::I64(n)) => n.to_string(),
        Some(Value::F64(f)) => f.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => type_hint(other).to_string(),
    }
}

/// A human label for an entity node: the first `name`/`title`/`label`
/// string property, else the first string property, else empty (the UI then
/// falls back to a short id).
fn entity_label(e: &EntityRecord, names: &Names) -> String {
    for (pid, v) in &e.properties {
        if let (Some(n), Value::String(s)) = (names.prop_name.get(&pid.get()), v) {
            let nl = n.to_ascii_lowercase();
            if nl == "name" || nl == "title" || nl == "label" {
                return s.clone();
            }
        }
    }
    for (_, v) in &e.properties {
        if let Value::String(s) = v {
            return s.clone();
        }
    }
    String::new()
}

/// Numeric coercion for `sum` aggregation; non-numbers contribute 0.
fn numeric(v: &Value) -> f64 {
    match v {
        #[allow(clippy::cast_precision_loss)]
        Value::I64(n) => *n as f64,
        Value::F64(f) => *f,
        _ => 0.0,
    }
}

/// Browsing parameters for [`Store::table`]: snapshot, paging, sort, and
/// filtering (a global substring `q` plus per-property `(name, substring)`).
#[derive(Default)]
pub struct TableQuery {
    /// Snapshot tx (None = head).
    pub as_of: Option<u64>,
    /// Page size.
    pub limit: usize,
    /// Rows to skip before the page.
    pub offset: usize,
    /// Property name to sort by (None = snapshot/insertion order).
    pub sort: Option<String>,
    /// Sort descending when true.
    pub desc: bool,
    /// Global case-insensitive substring across all columns.
    pub q: Option<String>,
    /// Per-column `(property_name, substring)` filters, AND-combined.
    pub filters: Vec<(String, String)>,
}

impl TableQuery {
    /// A plain page request (no sort/filter).
    #[must_use]
    pub fn new(as_of: Option<u64>, limit: usize) -> Self {
        Self { as_of, limit, ..Default::default() }
    }
}

/// Name dictionaries resolved from the current snapshot.
struct Names {
    type_name: BTreeMap<u32, String>,
    prop_name: BTreeMap<u32, String>,
    type_id: HashMap<String, u32>,
    prop_id: HashMap<String, u32>,
    max_type: u32,
    max_prop: u32,
}

impl Store {
    /// Wrap an opened engine.
    #[must_use]
    pub fn new(engine: SharedEngine) -> Self {
        Self { engine }
    }

    /// Latest committed transaction id (the default "now" snapshot).
    #[must_use]
    pub fn head(&self) -> u64 {
        self.engine.manifest_snapshot().last_tx_id
    }

    fn snapshot(&self, as_of: Option<u64>) -> TxId {
        TxId::new(as_of.unwrap_or_else(|| self.head()))
    }

    fn records(&self, snap: TxId) -> Vec<Record> {
        self.engine.snapshot_iter(snap).unwrap_or_default()
    }

    fn names(recs: &[Record]) -> Names {
        let mut n = Names {
            type_name: BTreeMap::new(),
            prop_name: BTreeMap::new(),
            type_id: HashMap::new(),
            prop_id: HashMap::new(),
            max_type: 0,
            max_prop: 0,
        };
        for r in recs {
            match r {
                Record::TypeName(t) => {
                    let id = t.id.get();
                    n.type_name.insert(id, t.name.clone());
                    n.type_id.insert(t.name.clone(), id);
                    n.max_type = n.max_type.max(id);
                }
                Record::PropertyKey(p) => {
                    let id = p.id.get();
                    n.prop_name.insert(id, p.name.clone());
                    n.prop_id.insert(p.name.clone(), id);
                    n.max_prop = n.max_prop.max(id);
                }
                _ => {}
            }
        }
        n
    }

    // ---- read paths -----------------------------------------------------

    /// Catalog: kinds present (with counts) and their properties (with names
    /// and an inferred type hint), plus the head transaction for the slider.
    #[must_use]
    pub fn catalog(&self, as_of: Option<u64>) -> J {
        let snap = self.snapshot(as_of);
        let recs = self.records(snap);
        let names = Self::names(&recs);

        // type_id -> (count, prop_id -> hint)
        let mut kinds: BTreeMap<u32, (u64, BTreeMap<u32, &'static str>)> = BTreeMap::new();
        for r in &recs {
            if let Record::Entity(e) = r {
                let entry = kinds.entry(e.type_id.get()).or_default();
                entry.0 += 1;
                for (pid, val) in &e.properties {
                    entry.1.entry(pid.get()).or_insert_with(|| type_hint(val));
                }
            }
        }

        let kinds_json: Vec<J> = kinds
            .iter()
            .filter_map(|(tid, (count, props))| {
                let name = names.type_name.get(tid).cloned()
                    .unwrap_or_else(|| format!("kind:{tid}"));
                if is_reserved(&name) {
                    return None; // hide $User and friends
                }
                let props_json: Vec<J> = props
                    .iter()
                    .filter_map(|(pid, hint)| {
                        let pname = names.prop_name.get(pid).cloned()
                            .unwrap_or_else(|| format!("prop:{pid}"));
                        if is_reserved(&pname) {
                            return None; // hide $author
                        }
                        Some(json!({ "property_id": pid, "name": pname, "type": hint }))
                    })
                    .collect();
                Some(json!({
                    "type_id": tid,
                    "name": name,
                    "count": count,
                    "properties": props_json,
                }))
            })
            .collect();

        json!({ "head": self.head(), "as_of": snap.get(), "kinds": kinds_json })
    }

    /// One record-kind projected to a table page: a header row of property
    /// names and the requested slice of rows (each carrying its `id`), after
    /// applying the query's filters and sort. `total` is the filtered count
    /// (for pagination); `shown` is the page size.
    #[must_use]
    pub fn table(&self, type_id: u32, q: &TableQuery) -> J {
        let snap = self.snapshot(q.as_of);
        let recs = self.records(snap);
        let names = Self::names(&recs);
        let tid = TypeId::new(type_id);

        // Reserved kinds ($User, …) are never exposed as a table — even by a
        // hand-crafted type_id — so account records stay hidden.
        if names.type_name.get(&type_id).is_some_and(|n| is_reserved(n)) {
            return json!({
                "type_id": type_id, "as_of": snap.get(), "headers": [], "rows": [],
                "total": 0, "shown": 0, "offset": 0, "limit": q.limit,
            });
        }

        let mut cols: BTreeSet<u32> = BTreeSet::new();
        let mut entities: Vec<&EntityRecord> = Vec::new();
        for r in &recs {
            if let Record::Entity(e) = r
                && e.type_id == tid
            {
                for (pid, _) in &e.properties {
                    let reserved = names.prop_name.get(&pid.get()).is_some_and(|n| is_reserved(n));
                    if !reserved {
                        cols.insert(pid.get());
                    }
                }
                entities.push(e);
            }
        }
        let cols: Vec<u32> = cols.into_iter().collect();

        // Display string of one property on an entity (drives filter + sort).
        let disp = |e: &EntityRecord, pid: u32| cell_key(prop_val(e, pid));

        // Filter: global substring across columns + per-column substrings.
        let ql = q.q.as_ref().map(|s| s.to_lowercase());
        let col_filters: Vec<(u32, String)> = q.filters.iter()
            .filter_map(|(name, val)| names.prop_id.get(name).map(|p| (*p, val.to_lowercase())))
            .collect();
        entities.retain(|e| {
            if let Some(ql) = &ql
                && !cols.iter().any(|&pid| disp(e, pid).to_lowercase().contains(ql))
            {
                return false;
            }
            col_filters.iter().all(|(pid, val)| disp(e, *pid).to_lowercase().contains(val))
        });

        // Sort by a property's display string (stable; case-insensitive).
        if let Some(sp) = q.sort.as_ref()
            && let Some(&pid) = names.prop_id.get(sp)
        {
            entities.sort_by_cached_key(|e| disp(e, pid).to_lowercase());
            if q.desc {
                entities.reverse();
            }
        }

        let total = entities.len();
        let headers: Vec<J> = cols
            .iter()
            .map(|pid| json!({
                "property_id": pid,
                "name": names.prop_name.get(pid).cloned().unwrap_or_else(|| format!("prop:{pid}")),
            }))
            .collect();

        let rows: Vec<J> = entities
            .iter()
            .skip(q.offset)
            .take(q.limit)
            .map(|e| {
                let by_prop: HashMap<u32, &Value> =
                    e.properties.iter().map(|(p, v)| (p.get(), v)).collect();
                let cells: Vec<J> = cols
                    .iter()
                    .map(|pid| by_prop.get(pid).map_or(J::Null, |v| to_json(v)))
                    .collect();
                json!({ "id": e.entity_id.into_uuid().to_string(), "cells": cells })
            })
            .collect();

        json!({
            "type_id": type_id,
            "as_of": snap.get(),
            "headers": headers,
            "rows": rows,
            "total": total,
            "shown": rows.len(),
            "offset": q.offset,
            "limit": q.limit,
        })
    }

    /// A single record with its full property list (names + values), or `null`.
    #[must_use]
    pub fn record(&self, id: Uuid, as_of: Option<u64>) -> J {
        let snap = self.snapshot(as_of);
        let recs = self.records(snap);
        let names = Self::names(&recs);
        match self.engine.snapshot_read(&id, snap) {
            Ok(Resolved::Live(Record::Entity(e)))
                if !names.type_name.get(&e.type_id.get()).is_some_and(|n| is_reserved(n)) =>
            {
                let props: Vec<J> = e
                    .properties
                    .iter()
                    .filter_map(|(pid, v)| {
                        let name = names.prop_name.get(&pid.get()).cloned()
                            .unwrap_or_else(|| format!("prop:{}", pid.get()));
                        if is_reserved(&name) {
                            return None;
                        }
                        Some(json!({
                            "property_id": pid.get(),
                            "name": name,
                            "value": to_json(v),
                            "type": type_hint(v),
                        }))
                    })
                    .collect();
                json!({
                    "id": id.to_string(),
                    "type_id": e.type_id.get(),
                    "kind": names.type_name.get(&e.type_id.get()).cloned()
                        .unwrap_or_else(|| format!("kind:{}", e.type_id.get())),
                    "asserted_at": e.tx_id_assert.get(),
                    "author": prop_val(&e, names.prop_id.get(AUTHOR_PROP).copied().unwrap_or(0))
                        .and_then(|v| if let Value::String(s) = v { Some(s.clone()) } else { None }),
                    "properties": props,
                })
            }
            _ => J::Null,
        }
    }

    /// History of one record. With `property` set, returns that property's
    /// change timeline (one entry per tx where the value actually changed,
    /// plus a terminal `deleted` entry for a tombstone) — the per-cell popover.
    /// With `property` `None`, returns every version's full property snapshot.
    ///
    /// Backed by the engine's version-chain walk, so it costs O(versions),
    /// not O(head).
    #[must_use]
    pub fn history(&self, id: Uuid, property: Option<&str>) -> J {
        let recs = self.records(self.snapshot(None));
        let names = Self::names(&recs);
        let versions = self.engine.versions_of(&id).unwrap_or_default();
        let author_pid = names.prop_id.get(AUTHOR_PROP).copied();
        let author_of = |e: &EntityRecord| -> Option<String> {
            author_pid
                .and_then(|ap| prop_val(e, ap))
                .and_then(|v| if let Value::String(s) = v { Some(s.clone()) } else { None })
        };

        if let Some(prop) = property {
            let pid = names.prop_id.get(prop).copied();
            let mut out: Vec<J> = Vec::new();
            let mut last: Option<J> = None;
            for (tx, rec) in &versions {
                match rec {
                    Record::Entity(e) => {
                        let v = pid
                            .and_then(|pid| {
                                e.properties.iter().find(|(p, _)| p.get() == pid)
                            })
                            .map_or(J::Null, |(_, v)| to_json(v));
                        if last.as_ref() != Some(&v) {
                            out.push(json!({ "tx": tx.get(), "value": v, "author": author_of(e) }));
                            last = Some(v);
                        }
                    }
                    Record::Tombstone(_) => {
                        out.push(json!({ "tx": tx.get(), "deleted": true }));
                        last = None;
                    }
                    _ => {}
                }
            }
            return json!({
                "id": id.to_string(),
                "property": prop,
                "property_id": pid,
                "versions": out,
            });
        }

        let out: Vec<J> = versions
            .iter()
            .map(|(tx, rec)| match rec {
                Record::Entity(e) => {
                    let props: Vec<J> = e
                        .properties
                        .iter()
                        .filter_map(|(pid, v)| {
                            let name = names.prop_name.get(&pid.get()).cloned()
                                .unwrap_or_else(|| format!("prop:{}", pid.get()));
                            if is_reserved(&name) {
                                return None;
                            }
                            Some(json!({ "name": name, "value": to_json(v) }))
                        })
                        .collect();
                    json!({ "tx": tx.get(), "author": author_of(e), "properties": props })
                }
                Record::Tombstone(_) => json!({ "tx": tx.get(), "deleted": true }),
                _ => json!({ "tx": tx.get() }),
            })
            .collect();
        json!({ "id": id.to_string(), "versions": out })
    }

    /// A 2-D pivot of one record-kind: group entities by `row_prop` × `col_prop`
    /// and aggregate each cell. `agg` is `"count"` (default) or `"sum"` over the
    /// numeric `value_prop`. Returns the distinct row/col values (as display
    /// strings), the cell matrix, and margins.
    #[must_use]
    pub fn pivot(
        &self,
        type_id: u32,
        row_prop: &str,
        col_prop: &str,
        agg: &str,
        value_prop: Option<&str>,
        as_of: Option<u64>,
    ) -> J {
        let snap = self.snapshot(as_of);
        let recs = self.records(snap);
        let names = Self::names(&recs);
        let tid = TypeId::new(type_id);
        let reserved_kind = names.type_name.get(&type_id).is_some_and(|n| is_reserved(n));
        let row_pid = names.prop_id.get(row_prop).copied();
        let col_pid = names.prop_id.get(col_prop).copied();
        let val_pid = value_prop.and_then(|p| names.prop_id.get(p).copied());
        let summing = agg == "sum";

        // (row, col) -> aggregate; plus ordered distinct row/col values.
        let mut rows: BTreeSet<String> = BTreeSet::new();
        let mut cols: BTreeSet<String> = BTreeSet::new();
        let mut cells: BTreeMap<(String, String), f64> = BTreeMap::new();
        for r in &recs {
            let Record::Entity(e) = r else { continue };
            if reserved_kind || e.type_id != tid {
                continue;
            }
            let rk = cell_key(row_pid.and_then(|p| prop_val(e, p)));
            let ck = cell_key(col_pid.and_then(|p| prop_val(e, p)));
            let amount = if summing {
                val_pid.and_then(|p| prop_val(e, p)).map_or(0.0, numeric)
            } else {
                1.0
            };
            rows.insert(rk.clone());
            cols.insert(ck.clone());
            *cells.entry((rk, ck)).or_insert(0.0) += amount;
        }

        let row_vals: Vec<String> = rows.into_iter().collect();
        let col_vals: Vec<String> = cols.into_iter().collect();
        let matrix: Vec<Vec<f64>> = row_vals
            .iter()
            .map(|rk| {
                col_vals
                    .iter()
                    .map(|ck| *cells.get(&(rk.clone(), ck.clone())).unwrap_or(&0.0))
                    .collect()
            })
            .collect();
        let row_totals: Vec<f64> = matrix.iter().map(|row| row.iter().sum()).collect();
        let col_totals: Vec<f64> = (0..col_vals.len())
            .map(|c| matrix.iter().map(|row| row[c]).sum())
            .collect();
        let grand_total: f64 = row_totals.iter().sum();

        json!({
            "type_id": type_id,
            "as_of": snap.get(),
            "row_prop": row_prop,
            "col_prop": col_prop,
            "agg": if summing { "sum" } else { "count" },
            "value_prop": value_prop,
            "rows": row_vals,
            "cols": col_vals,
            "cells": matrix,
            "row_totals": row_totals,
            "col_totals": col_totals,
            "grand_total": grand_total,
        })
    }

    /// A graph projection of the database: entities become nodes; edges come
    /// from `EntityRef`-valued properties (a directed `ref` link labelled by
    /// the property) and from hyperedges. A binary hyperedge is one link
    /// between its two members; an N-ary (>2) hyperedge becomes its own node
    /// with a star of links to each member — so N-ary structure is shown
    /// faithfully rather than flattened into pairwise edges.
    ///
    /// Entities are capped at `limit` (deterministic snapshot order); links
    /// are kept only between included nodes.
    #[must_use]
    pub fn graph(&self, as_of: Option<u64>, limit: usize) -> J {
        let snap = self.snapshot(as_of);
        let recs = self.records(snap);
        let names = Self::names(&recs);

        let mut included: BTreeSet<Uuid> = BTreeSet::new();
        let mut nodes: Vec<J> = Vec::new();
        let mut total_entities = 0usize;
        for r in &recs {
            let Record::Entity(e) = r else { continue };
            let kind = names.type_name.get(&e.type_id.get()).cloned()
                .unwrap_or_else(|| format!("kind:{}", e.type_id.get()));
            if is_reserved(&kind) {
                continue; // user/account records are not graph data
            }
            total_entities += 1;
            if included.len() >= limit {
                continue;
            }
            let id = e.entity_id.into_uuid();
            if included.insert(id) {
                nodes.push(json!({
                    "id": id.to_string(),
                    "type_id": e.type_id.get(),
                    "kind": kind,
                    "label": entity_label(e, &names),
                }));
            }
        }

        let mut links: Vec<J> = Vec::new();
        for r in &recs {
            match r {
                Record::Entity(e) => {
                    let sid = e.entity_id.into_uuid();
                    if !included.contains(&sid) {
                        continue;
                    }
                    for (pid, v) in &e.properties {
                        if let Value::EntityRef(t) = v {
                            let tid = t.into_uuid();
                            if included.contains(&tid) {
                                links.push(json!({
                                    "source": sid.to_string(),
                                    "target": tid.to_string(),
                                    "kind": "ref",
                                    "label": names.prop_name.get(&pid.get()).cloned()
                                        .unwrap_or_else(|| format!("prop:{}", pid.get())),
                                }));
                            }
                        }
                    }
                }
                Record::HyperEdge(h) => {
                    let members: Vec<String> = h
                        .roles
                        .iter()
                        .map(|(_, eid)| eid.into_uuid())
                        .filter(|id| included.contains(id))
                        .map(|id| id.to_string())
                        .collect();
                    if members.len() < 2 {
                        continue;
                    }
                    let label = names.type_name.get(&h.type_id.get()).cloned()
                        .unwrap_or_else(|| format!("edge:{}", h.type_id.get()));
                    if members.len() == 2 {
                        links.push(json!({
                            "source": members[0], "target": members[1],
                            "kind": "hyperedge", "label": label,
                        }));
                    } else {
                        let hid = h.hyperedge_id.into_uuid().to_string();
                        nodes.push(json!({
                            "id": hid, "type_id": h.type_id.get(),
                            "kind": label, "label": label, "hyper": true,
                        }));
                        for m in &members {
                            links.push(json!({
                                "source": hid, "target": m,
                                "kind": "hyperedge", "label": label,
                            }));
                        }
                    }
                }
                _ => {}
            }
        }

        json!({
            "as_of": snap.get(),
            "nodes": nodes,
            "links": links,
            "total_entities": total_entities,
            "shown": included.len(),
            "truncated": total_entities > included.len(),
        })
    }

    /// Run a read-only nDB query (the `ndb-query` language) against the current
    /// snapshot. Returns `Ok({columns, rows, truncated})` or `Err(envelope)`
    /// where the envelope is a `{error, code, detail, span?}` object the caller
    /// surfaces as a 4xx. Write clauses are rejected by the read executor.
    ///
    /// # Errors
    /// Returns the query-error envelope as `Err`.
    pub fn query(&self, text: &str) -> Result<J, J> {
        let guard = self.engine.raw_lock().read().expect("engine lock poisoned");
        match ndb_query::run::execute_text_read(&guard, text) {
            Ok(resp) => Ok(json!({
                "columns": resp.columns,
                "rows": resp.rows,
                "truncated": resp.truncated,
            })),
            Err(e) => Err(serde_json::to_value(e.envelope()).unwrap_or_else(|_| {
                json!({ "error": "query", "code": e.code(), "detail": e.to_string() })
            })),
        }
    }

    // ---- write paths ----------------------------------------------------

    /// Create a new entity of `kind` with the given `(property_name, value)`
    /// pairs. Unknown kind/property names are interned. When `author` is set
    /// (and `kind` is not reserved), a hidden `$author` property records who
    /// wrote this version. Returns the new tx id.
    ///
    /// # Errors
    /// Propagates engine errors (including `WriteStalled` under backpressure).
    pub fn create(
        &self,
        kind: &str,
        props: &[(String, Value)],
        author: Option<&str>,
    ) -> Result<u64, StoreError> {
        let recs = self.records(self.snapshot(None));
        let names = Self::names(&recs);
        let mut alloc = Allocator::new(&names);

        let tx = self.engine.with_write_txn(|mut txn| {
            let type_id = alloc.type_id(kind, &mut txn);
            let mut entity_props = Vec::with_capacity(props.len() + 1);
            for (name, value) in props {
                let pid = alloc.prop_id(name, &mut txn);
                entity_props.push((pid, value.clone()));
            }
            if let Some(a) = author
                && !is_reserved(kind)
            {
                let apid = alloc.prop_id(AUTHOR_PROP, &mut txn);
                entity_props.push((apid, Value::String(a.to_string())));
            }
            txn.put_entity(EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id,
                tx_id_assert: TxId::ACTIVE,
                tx_id_supersede: TxId::ACTIVE,
                properties: entity_props,
            });
            txn.commit()
        })?;
        Ok(tx.get())
    }

    /// Set `property` on an existing record to `value`, committing a new
    /// version. Returns the new tx id.
    ///
    /// # Errors
    /// Returns `Err` if the record does not exist or on an engine error.
    pub fn set(
        &self,
        id: Uuid,
        property: &str,
        value: &Value,
        author: Option<&str>,
    ) -> Result<u64, StoreError> {
        let snap = self.snapshot(None);
        let recs = self.records(snap);
        let names = Self::names(&recs);
        let mut alloc = Allocator::new(&names);

        let Resolved::Live(Record::Entity(current)) = self.engine.snapshot_read(&id, snap)? else {
            return Err(StoreError::NotFound);
        };
        let kind_reserved = names.type_name.get(&current.type_id.get())
            .is_some_and(|n| is_reserved(n));

        let tx = self.engine.with_write_txn(|mut txn| {
            let pid = alloc.prop_id(property, &mut txn);
            let mut props = current.properties.clone();
            match props.iter_mut().find(|(p, _)| *p == pid) {
                Some(slot) => slot.1 = value.clone(),
                None => props.push((pid, value.clone())),
            }
            if let Some(a) = author
                && !kind_reserved
            {
                let apid = alloc.prop_id(AUTHOR_PROP, &mut txn);
                let av = Value::String(a.to_string());
                match props.iter_mut().find(|(p, _)| *p == apid) {
                    Some(slot) => slot.1 = av,
                    None => props.push((apid, av)),
                }
            }
            txn.put_entity(EntityRecord {
                entity_id: current.entity_id,
                type_id: current.type_id,
                tx_id_assert: TxId::ACTIVE,
                tx_id_supersede: TxId::ACTIVE,
                properties: props,
            });
            txn.commit()
        })?;
        Ok(tx.get())
    }

    /// Tombstone a record (it stays in history). Returns the new tx id.
    ///
    /// # Errors
    /// Propagates engine errors.
    pub fn delete(&self, id: Uuid) -> Result<u64, StoreError> {
        let tx = self.engine.with_write_txn(|mut txn| {
            txn.delete(id);
            txn.commit()
        })?;
        Ok(tx.get())
    }

    // ---- user accounts (stored as reserved `$User` records) -------------

    /// Whether any user account exists — drives one-time admin bootstrap.
    #[must_use]
    pub fn has_any_user(&self) -> bool {
        let recs = self.records(self.snapshot(None));
        let names = Self::names(&recs);
        let Some(uid) = names.type_id.get(USER_KIND).copied() else {
            return false;
        };
        recs.iter().any(|r| matches!(r, Record::Entity(e) if e.type_id.get() == uid))
    }

    /// Look up a user by name → `(entity id, password hash, role string)`.
    #[must_use]
    pub fn find_user(&self, username: &str) -> Option<(Uuid, String, String)> {
        let recs = self.records(self.snapshot(None));
        let names = Self::names(&recs);
        let uid = names.type_id.get(USER_KIND).copied()?;
        let un_pid = names.prop_id.get("username").copied()?;
        let pw_pid = names.prop_id.get("pwhash").copied();
        let role_pid = names.prop_id.get("role").copied();
        for r in &recs {
            let Record::Entity(e) = r else { continue };
            if e.type_id.get() != uid {
                continue;
            }
            if prop_val(e, un_pid).and_then(str_of).as_deref() == Some(username) {
                let pw = pw_pid.and_then(|p| prop_val(e, p)).and_then(str_of).unwrap_or_default();
                let role = role_pid.and_then(|p| prop_val(e, p)).and_then(str_of)
                    .unwrap_or_else(|| "viewer".to_string());
                return Some((e.entity_id.into_uuid(), pw, role));
            }
        }
        None
    }

    /// All accounts as `(username, role)`, sorted by username.
    #[must_use]
    pub fn list_users(&self) -> Vec<(String, String)> {
        let recs = self.records(self.snapshot(None));
        let names = Self::names(&recs);
        let Some(uid) = names.type_id.get(USER_KIND).copied() else {
            return Vec::new();
        };
        let un_pid = names.prop_id.get("username").copied();
        let role_pid = names.prop_id.get("role").copied();
        let mut out: Vec<(String, String)> = recs
            .iter()
            .filter_map(|r| {
                let Record::Entity(e) = r else { return None };
                if e.type_id.get() != uid {
                    return None;
                }
                let name = un_pid.and_then(|p| prop_val(e, p)).and_then(str_of)?;
                let role = role_pid.and_then(|p| prop_val(e, p)).and_then(str_of)
                    .unwrap_or_else(|| "viewer".to_string());
                Some((name, role))
            })
            .collect();
        out.sort();
        out
    }

    /// Create a user account. Errors if the username already exists.
    ///
    /// # Errors
    /// `BadValue` if the username is taken; engine errors otherwise.
    pub fn create_user(&self, username: &str, pwhash: &str, role: &str) -> Result<u64, StoreError> {
        if username.is_empty() {
            return Err(StoreError::BadValue("username required".to_string()));
        }
        if self.find_user(username).is_some() {
            return Err(StoreError::BadValue(format!("user {username} already exists")));
        }
        self.create(
            USER_KIND,
            &[
                ("username".to_string(), Value::String(username.to_string())),
                ("pwhash".to_string(), Value::String(pwhash.to_string())),
                ("role".to_string(), Value::String(role.to_string())),
            ],
            None,
        )
    }

    /// Delete a user account by username.
    ///
    /// # Errors
    /// `NotFound` if no such user; engine errors otherwise.
    pub fn delete_user(&self, username: &str) -> Result<u64, StoreError> {
        let (id, _, _) = self.find_user(username).ok_or(StoreError::NotFound)?;
        self.delete(id)
    }

    // ---- replication (leader / follower over the engine's WAL stream) ---

    /// This node's replication status: head tx, `SSTable` count, active WAL seq.
    #[must_use]
    pub fn replication_status(&self) -> J {
        let wal_seq = self.engine.raw_lock().read().expect("engine lock poisoned").active_wal_seq();
        json!({ "head": self.head(), "sstables": self.engine.sstable_count(), "wal_seq": wal_seq })
    }

    /// **Leader side.** Stream the WAL delta for `(seq, after)` as a base64
    /// record batch plus the cursor a follower advances with. Mirrors
    /// `Engine::serve_replication`.
    #[must_use]
    pub fn serve_replication(&self, seq: u64, after: u64) -> J {
        let guard = self.engine.raw_lock().read().expect("engine lock poisoned");
        match guard.serve_replication(seq, after) {
            Ok(b) => json!({
                "current_wal_seq": b.current_wal_seq,
                "available": b.available,
                "segment_sealed": b.segment_sealed,
                "next_wal_seq": b.next_wal_seq,
                "next_offset": b.next_offset,
                "count": b.records.len(),
                "records_b64": ndb_engine::replication::encode_records_b64(&b.records),
            }),
            Err(e) => json!({ "error": { "code": "engine_error", "message": format!("{e}") } }),
        }
    }

    /// **Follower side.** Decode a base64 record batch and ingest it verbatim
    /// (leader tx ids preserved). Returns the new head tx.
    ///
    /// # Errors
    /// `BadValue` on a malformed batch; engine errors on apply.
    pub fn ingest_replicated_b64(&self, records_b64: &str) -> Result<u64, StoreError> {
        let records = ndb_engine::replication::decode_records_b64(records_b64)
            .map_err(|e| StoreError::BadValue(format!("decode batch: {e}")))?;
        {
            let mut guard = self.engine.raw_lock().write().expect("engine lock poisoned");
            guard.ingest_replicated(records)?;
        }
        Ok(self.head())
    }
}

/// Resolves names to ids within a write transaction, allocating + writing a
/// `TypeName` / `PropertyKey` record for any name not yet in the dictionary.
struct Allocator {
    type_id: HashMap<String, u32>,
    prop_id: HashMap<String, u32>,
    next_type: u32,
    next_prop: u32,
}

impl Allocator {
    fn new(names: &Names) -> Self {
        Self {
            type_id: names.type_id.clone(),
            prop_id: names.prop_id.clone(),
            next_type: names.max_type + 1,
            next_prop: names.max_prop + 1,
        }
    }

    fn type_id(&mut self, name: &str, txn: &mut ndb_engine::engine::WriteTxn<'_>) -> TypeId {
        if let Some(id) = self.type_id.get(name) {
            return TypeId::new(*id);
        }
        let id = self.next_type;
        self.next_type += 1;
        self.type_id.insert(name.to_string(), id);
        txn.put_raw(Record::TypeName(TypeNameRecord {
            id: TypeId::new(id),
            name: name.to_string(),
        }));
        TypeId::new(id)
    }

    fn prop_id(&mut self, name: &str, txn: &mut ndb_engine::engine::WriteTxn<'_>) -> PropertyId {
        if let Some(id) = self.prop_id.get(name) {
            return PropertyId::new(*id);
        }
        let id = self.next_prop;
        self.next_prop += 1;
        self.prop_id.insert(name.to_string(), id);
        txn.put_raw(Record::PropertyKey(PropertyKeyRecord {
            id: PropertyId::new(id),
            name: name.to_string(),
        }));
        PropertyId::new(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndb_engine::value::Value;

    /// A fresh, empty Store backed by a temp on-disk engine.
    fn fresh() -> Store {
        let dir = std::env::temp_dir().join(format!("ndb-studio-test-{}", Uuid::now_v7()));
        let engine = SharedEngine::create(&dir).expect("create engine");
        Store::new(engine)
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    /// Creating records interns the kind + property names and the catalog +
    /// table project them back as familiar rows (not raw `kind:N` ids).
    #[test]
    fn create_then_catalog_and_table() {
        let store = fresh();
        store
            .create("Person", &[("name".into(), s("Alice")), ("age".into(), Value::I64(30))], None)
            .expect("create alice");
        store
            .create("Person", &[("name".into(), s("Bob")), ("age".into(), Value::I64(25))], None)
            .expect("create bob");

        let cat = store.catalog(None);
        let kinds = cat["kinds"].as_array().unwrap();
        assert_eq!(kinds.len(), 1, "one kind");
        assert_eq!(kinds[0]["name"], "Person");
        assert_eq!(kinds[0]["count"], 2);
        let prop_names: Vec<&str> = kinds[0]["properties"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(prop_names.contains(&"name") && prop_names.contains(&"age"));

        let tid = u32::try_from(kinds[0]["type_id"].as_u64().unwrap()).unwrap();
        let table = store.table(tid, &TableQuery::new(None, 1000));
        assert_eq!(table["total"], 2);
        assert_eq!(table["rows"].as_array().unwrap().len(), 2);
    }

    /// `set` commits a new version; the old value is still readable as-of an
    /// earlier transaction, on both the table and single-record paths.
    #[test]
    fn edit_creates_version_and_time_travel() {
        let store = fresh();
        let tx1 = store
            .create("Person", &[("age".into(), Value::I64(30))], None)
            .expect("create");
        let tid = u32::try_from(store.catalog(None)["kinds"][0]["type_id"].as_u64().unwrap()).unwrap();
        let id_str = store.table(tid, &TableQuery::new(None, 10))["rows"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let id = Uuid::parse_str(&id_str).unwrap();

        store.set(id, "age", &Value::I64(31), None).expect("set");

        // Now: 31. As of the create tx: 30.
        let now = store.record(id, None);
        assert_eq!(now["properties"][0]["value"], 31);
        let past = store.record(id, Some(tx1));
        assert_eq!(past["properties"][0]["value"], 30);

        // Table path honours the same snapshot.
        let past_table = store.table(tid, &TableQuery::new(Some(tx1), 10));
        assert_eq!(past_table["rows"][0]["cells"][0], 30);
    }

    /// `delete` tombstones at head but the record remains visible in history.
    #[test]
    fn delete_tombstones_but_history_remains() {
        let store = fresh();
        let tx_create = store
            .create("Note", &[("body".into(), s("hi"))], None)
            .expect("create");
        let tid = u32::try_from(store.catalog(None)["kinds"][0]["type_id"].as_u64().unwrap()).unwrap();
        let id = Uuid::parse_str(store.table(tid, &TableQuery::new(None, 10))["rows"][0]["id"].as_str().unwrap())
            .unwrap();

        store.delete(id).expect("delete");

        assert_eq!(store.table(tid, &TableQuery::new(None, 10))["total"], 0, "gone at head");
        assert_eq!(
            store.table(tid, &TableQuery::new(Some(tx_create), 10))["total"],
            1,
            "still in history"
        );
        assert!(store.record(id, None).is_null(), "no live record at head");
        assert!(!store.record(id, Some(tx_create)).is_null(), "live in history");
    }

    /// The per-cell history is the property's change timeline (deduped) ending
    /// in a `deleted` entry, and is unaffected by edits to other properties.
    #[test]
    fn history_is_property_change_timeline() {
        let store = fresh();
        store
            .create("Person", &[("name".into(), s("Alice")), ("age".into(), Value::I64(30))], None)
            .expect("create");
        let tid = u32::try_from(store.catalog(None)["kinds"][0]["type_id"].as_u64().unwrap())
            .unwrap();
        let id = Uuid::parse_str(store.table(tid, &TableQuery::new(None, 10))["rows"][0]["id"].as_str().unwrap())
            .unwrap();

        store.set(id, "age", &Value::I64(31), None).expect("set 31");
        store.set(id, "name", &s("Alicia"), None).expect("set unrelated"); // must not appear in age history
        store.set(id, "age", &Value::I64(32), None).expect("set 32");
        store.delete(id).expect("delete");

        let h = store.history(id, Some("age"));
        let vs = h["versions"].as_array().unwrap();
        // 30, 31, 32, deleted — the name edit is invisible to the age timeline.
        assert_eq!(vs.len(), 4, "three values + a delete");
        assert_eq!(vs[0]["value"], 30);
        assert_eq!(vs[1]["value"], 31);
        assert_eq!(vs[2]["value"], 32);
        assert_eq!(vs[3]["deleted"], true);

        // Whole-record history (no property) yields one entry per version.
        let whole = store.history(id, None);
        assert_eq!(whole["versions"].as_array().unwrap().len(), 5);
    }

    /// A pivot groups by row × col and aggregates count and sum with margins.
    #[test]
    fn pivot_counts_and_sums_with_margins() {
        let store = fresh();
        for (name, city, status, age) in [
            ("Alice", "Hanoi", "active", 30),
            ("Bob", "Hanoi", "inactive", 40),
            ("Cara", "HCMC", "active", 25),
        ] {
            store
                .create(
                    "Person",
                    &[
                        ("name".into(), s(name)),
                        ("city".into(), s(city)),
                        ("status".into(), s(status)),
                        ("age".into(), Value::I64(age)),
                    ],
                    None,
                )
                .expect("create");
        }
        let tid = u32::try_from(store.catalog(None)["kinds"][0]["type_id"].as_u64().unwrap())
            .unwrap();

        let p = store.pivot(tid, "city", "status", "count", None, None);
        // rows sorted: HCMC, Hanoi ; cols sorted: active, inactive
        assert_eq!(p["rows"], json!(["HCMC", "Hanoi"]));
        assert_eq!(p["cols"], json!(["active", "inactive"]));
        assert_eq!(p["cells"], json!([[1.0, 0.0], [1.0, 1.0]]));
        assert_eq!(p["grand_total"], 3.0);
        assert_eq!(p["row_totals"], json!([1.0, 2.0]));
        assert_eq!(p["col_totals"], json!([2.0, 1.0]));

        let sum = store.pivot(tid, "city", "status", "sum", Some("age"), None);
        // Hanoi/active=30, Hanoi/inactive=40, HCMC/active=25
        assert_eq!(sum["cells"], json!([[25.0, 0.0], [30.0, 40.0]]));
        assert_eq!(sum["grand_total"], 95.0);
    }

    /// The graph projection: `EntityRef` props are `ref` links, binary
    /// hyperedges are single links, and N-ary hyperedges become a hub node
    /// with a star of links.
    #[test]
    fn graph_projects_refs_and_hyperedges() {
        use ndb_engine::id::{HyperedgeId, RoleId};
        use ndb_engine::record::HyperEdgeRecord;

        let store = fresh();
        let (a, b, c, d) = (
            EntityId::now_v7(),
            EntityId::now_v7(),
            EntityId::now_v7(),
            EntityId::now_v7(),
        );
        store
            .engine
            .with_write_txn(|mut txn| {
                txn.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(1), name: "Person".into() }));
                txn.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(2), name: "Knows".into() }));
                txn.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(3), name: "Trio".into() }));
                txn.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(1), name: "name".into() }));
                txn.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(2), name: "friend".into() }));
                for (eid, nm, friend) in [(a, "A", Some(b)), (b, "B", None), (c, "C", None), (d, "D", None)] {
                    let mut props = vec![(PropertyId::new(1), Value::String(nm.into()))];
                    if let Some(f) = friend {
                        props.push((PropertyId::new(2), Value::EntityRef(f)));
                    }
                    txn.put_entity(EntityRecord {
                        entity_id: eid,
                        type_id: TypeId::new(1),
                        tx_id_assert: TxId::ACTIVE,
                        tx_id_supersede: TxId::ACTIVE,
                        properties: props,
                    });
                }
                // Binary hyperedge A—B, N-ary hyperedge B-C-D.
                txn.put_hyperedge(HyperEdgeRecord {
                    hyperedge_id: HyperedgeId::now_v7(),
                    type_id: TypeId::new(2),
                    tx_id_assert: TxId::ACTIVE,
                    tx_id_supersede: TxId::ACTIVE,
                    roles: vec![(RoleId::new(1), a), (RoleId::new(2), b)],
                    hyperedge_roles: vec![],
                    properties: vec![],
                });
                txn.put_hyperedge(HyperEdgeRecord {
                    hyperedge_id: HyperedgeId::now_v7(),
                    type_id: TypeId::new(3),
                    tx_id_assert: TxId::ACTIVE,
                    tx_id_supersede: TxId::ACTIVE,
                    roles: vec![(RoleId::new(1), b), (RoleId::new(2), c), (RoleId::new(3), d)],
                    hyperedge_roles: vec![],
                    properties: vec![],
                });
                txn.commit()
            })
            .expect("seed graph");

        let proj = store.graph(None, 300);
        let nodes = proj["nodes"].as_array().unwrap();
        let links = proj["links"].as_array().unwrap();
        // 4 entity nodes + 1 hub node for the N-ary edge.
        assert_eq!(proj["total_entities"], 4);
        assert_eq!(nodes.len(), 5);
        assert_eq!(nodes.iter().filter(|n| n["hyper"] == true).count(), 1, "one N-ary hub");
        // 1 ref + 1 binary hyperedge + 3 star links from the hub.
        assert_eq!(links.len(), 5);
        assert_eq!(links.iter().filter(|l| l["kind"] == "ref").count(), 1);
        assert_eq!(links.iter().filter(|l| l["kind"] == "hyperedge").count(), 4);
        assert_eq!(proj["truncated"], false);

        // Limit caps entities and drops links to excluded nodes.
        let capped = store.graph(None, 2);
        assert_eq!(capped["shown"], 2);
        assert_eq!(capped["truncated"], true);
    }

    /// User accounts live as hidden `$User` records; author attribution is a
    /// hidden `$author` property. Neither leaks into the data-facing views, but
    /// the author surfaces in history.
    #[test]
    fn users_and_author_are_hidden_from_data_views() {
        let store = fresh();
        assert!(!store.has_any_user());
        store.create_user("alice", "HASH", "editor").expect("create user");
        assert!(store.has_any_user());
        assert!(store.create_user("alice", "X", "viewer").is_err(), "duplicate rejected");

        let (_, hash, role) = store.find_user("alice").expect("found");
        assert_eq!(hash, "HASH");
        assert_eq!(role, "editor");
        assert_eq!(store.list_users(), vec![("alice".to_string(), "editor".to_string())]);

        // A record authored by alice.
        store
            .create("Person", &[("name".into(), s("Doc"))], Some("alice"))
            .expect("create");

        // Catalog shows only Person (not $User), and only the `name` property.
        let cat = store.catalog(None);
        let kinds = cat["kinds"].as_array().unwrap();
        assert_eq!(kinds.len(), 1, "$User kind hidden");
        assert_eq!(kinds[0]["name"], "Person");
        let props: Vec<&str> = kinds[0]["properties"].as_array().unwrap()
            .iter().map(|p| p["name"].as_str().unwrap()).collect();
        assert_eq!(props, vec!["name"], "$author property hidden");

        // Table for Person has no $author column.
        let tid = u32::try_from(kinds[0]["type_id"].as_u64().unwrap()).unwrap();
        let table = store.table(tid, &TableQuery::new(None, 10));
        let headers: Vec<&str> = table["headers"].as_array().unwrap()
            .iter().map(|h| h["name"].as_str().unwrap()).collect();
        assert_eq!(headers, vec!["name"]);

        // But history attributes the version to alice.
        let id = Uuid::parse_str(table["rows"][0]["id"].as_str().unwrap()).unwrap();
        let h = store.history(id, Some("name"));
        assert_eq!(h["versions"][0]["author"], "alice");

        // The $User record itself is never exposed via record() or table(),
        // even by its raw id / interned type_id (no pwhash leak).
        let (uid, _, _) = store.find_user("alice").unwrap();
        assert!(store.record(uid, None).is_null(), "user record hidden");
        assert_eq!(store.table(1, &TableQuery::new(None, 10))["total"], 0, "$User not tabled (type 1)");

        // Deleting a user removes it.
        store.delete_user("alice").expect("delete user");
        assert!(store.find_user("alice").is_none());
    }

    /// The query console runs read-only queries and returns a structured
    /// error envelope (with a span) on a parse failure.
    #[test]
    fn query_returns_rows_and_error_envelope() {
        let store = fresh();
        store.create("Person", &[("name".into(), s("Alice"))], None).expect("a");
        store.create("Person", &[("name".into(), s("Bob"))], None).expect("b");

        let ok = store.query("match Person(name: ?n) return ?n").expect("query ok");
        assert_eq!(ok["columns"], json!(["n"]));
        assert_eq!(ok["rows"].as_array().unwrap().len(), 2);

        let err = store.query("match Person(name: ?n return ?n").expect_err("parse error");
        assert_eq!(err["error"], "parse");
        assert!(err["span"].is_object(), "parse error carries a span");
    }

    /// A follower with an empty database replicates a leader's commits to
    /// byte-identical state by streaming the leader's WAL delta.
    #[test]
    fn replication_leader_to_follower_round_trip() {
        let leader = fresh();
        let follower = fresh();
        leader.create("Person", &[("name".into(), s("Alice"))], None).expect("a");
        leader.create("Person", &[("name".into(), s("Bob"))], None).expect("b");

        // Follower streams from the leader's active WAL segment, offset 0.
        let mut seq = leader.replication_status()["wal_seq"].as_u64().unwrap();
        let mut off = 0u64;
        for _ in 0..100 {
            let b = leader.serve_replication(seq, off);
            assert!(b["available"].as_bool().unwrap_or(false), "segment available");
            let n = b["count"].as_u64().unwrap();
            if n > 0 {
                follower.ingest_replicated_b64(b["records_b64"].as_str().unwrap()).expect("ingest");
            }
            off = b["next_offset"].as_u64().unwrap();
            if n == 0 {
                if b["segment_sealed"].as_bool() == Some(true)
                    && let Some(next) = b["next_wal_seq"].as_u64()
                {
                    seq = next;
                    off = 0;
                    continue;
                }
                break;
            }
        }

        // The follower now mirrors the leader.
        assert_eq!(follower.head(), leader.head(), "watermarks match");
        let cat = follower.catalog(None);
        assert_eq!(cat["kinds"][0]["name"], "Person");
        assert_eq!(cat["kinds"][0]["count"], 2);
    }

    /// Table browsing: per-column filter, global search, sort, and paging.
    #[test]
    fn table_filter_sort_paginate() {
        let store = fresh();
        for (n, c) in [("Alice", "Hanoi"), ("Bob", "Hanoi"), ("Cara", "HCMC"), ("Dan", "HCMC"), ("Eve", "Hue")] {
            store.create("Person", &[("name".into(), s(n)), ("city".into(), s(c))], None).expect("c");
        }
        let tid = u32::try_from(store.catalog(None)["kinds"][0]["type_id"].as_u64().unwrap()).unwrap();

        // Per-column filter: city contains "Ha" → the two Hanoi rows.
        let q = TableQuery { filters: vec![("city".into(), "Ha".into())], ..TableQuery::new(None, 50) };
        assert_eq!(store.table(tid, &q)["total"], 2);

        // Global search "ar" → Cara only.
        let q = TableQuery { q: Some("ar".into()), ..TableQuery::new(None, 50) };
        assert_eq!(store.table(tid, &q)["total"], 1);

        // Sort by name desc → Eve first.
        let q = TableQuery { sort: Some("name".into()), desc: true, ..TableQuery::new(None, 50) };
        assert_eq!(store.table(tid, &q)["rows"][0]["cells"][0], "Eve");

        // Page 2 of name-asc (limit 2, offset 2) → Cara, Dan.
        let q = TableQuery { sort: Some("name".into()), offset: 2, ..TableQuery::new(None, 2) };
        let p = store.table(tid, &q);
        assert_eq!(p["total"], 5);
        assert_eq!(p["shown"], 2);
        assert_eq!(p["rows"][0]["cells"][0], "Cara");
    }

    /// Editing a record that does not exist is a typed `NotFound` (HTTP 404).
    #[test]
    fn set_unknown_record_is_not_found() {
        let store = fresh();
        let err = store
            .set(Uuid::now_v7(), "x", &Value::I64(1), None)
            .expect_err("must fail");
        assert!(matches!(err, StoreError::NotFound));
        assert_eq!(err.status(), 404);
    }
}
