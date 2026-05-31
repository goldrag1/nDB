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
            .map(|(tid, (count, props))| {
                let props_json: Vec<J> = props
                    .iter()
                    .map(|(pid, hint)| {
                        json!({
                            "property_id": pid,
                            "name": names.prop_name.get(pid).cloned()
                                .unwrap_or_else(|| format!("prop:{pid}")),
                            "type": hint,
                        })
                    })
                    .collect();
                json!({
                    "type_id": tid,
                    "name": names.type_name.get(tid).cloned()
                        .unwrap_or_else(|| format!("kind:{tid}")),
                    "count": count,
                    "properties": props_json,
                })
            })
            .collect();

        json!({ "head": self.head(), "as_of": snap.get(), "kinds": kinds_json })
    }

    /// One record-kind projected to a table: a header row of property names and
    /// one row per entity (each row carries its `id` for edit/select).
    #[must_use]
    pub fn table(&self, type_id: u32, as_of: Option<u64>, limit: usize) -> J {
        let snap = self.snapshot(as_of);
        let recs = self.records(snap);
        let names = Self::names(&recs);
        let tid = TypeId::new(type_id);

        let mut cols: BTreeSet<u32> = BTreeSet::new();
        let mut entities: Vec<&EntityRecord> = Vec::new();
        for r in &recs {
            if let Record::Entity(e) = r
                && e.type_id == tid
            {
                for (pid, _) in &e.properties {
                    cols.insert(pid.get());
                }
                entities.push(e);
            }
        }
        let cols: Vec<u32> = cols.into_iter().collect();

        let headers: Vec<J> = cols
            .iter()
            .map(|pid| {
                json!({
                    "property_id": pid,
                    "name": names.prop_name.get(pid).cloned()
                        .unwrap_or_else(|| format!("prop:{pid}")),
                })
            })
            .collect();

        let rows: Vec<J> = entities
            .iter()
            .take(limit)
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
            "total": entities.len(),
            "shown": entities.len().min(limit),
        })
    }

    /// A single record with its full property list (names + values), or `null`.
    #[must_use]
    pub fn record(&self, id: Uuid, as_of: Option<u64>) -> J {
        let snap = self.snapshot(as_of);
        let recs = self.records(snap);
        let names = Self::names(&recs);
        match self.engine.snapshot_read(&id, snap) {
            Ok(Resolved::Live(Record::Entity(e))) => {
                let props: Vec<J> = e
                    .properties
                    .iter()
                    .map(|(pid, v)| {
                        json!({
                            "property_id": pid.get(),
                            "name": names.prop_name.get(&pid.get()).cloned()
                                .unwrap_or_else(|| format!("prop:{}", pid.get())),
                            "value": to_json(v),
                            "type": type_hint(v),
                        })
                    })
                    .collect();
                json!({
                    "id": id.to_string(),
                    "type_id": e.type_id.get(),
                    "kind": names.type_name.get(&e.type_id.get()).cloned()
                        .unwrap_or_else(|| format!("kind:{}", e.type_id.get())),
                    "asserted_at": e.tx_id_assert.get(),
                    "properties": props,
                })
            }
            _ => J::Null,
        }
    }

    // ---- write paths ----------------------------------------------------

    /// Create a new entity of `kind` with the given `(property_name, value)`
    /// pairs. Unknown kind/property names are interned. Returns the new tx id.
    ///
    /// # Errors
    /// Propagates engine errors (including `WriteStalled` under backpressure).
    pub fn create(&self, kind: &str, props: &[(String, Value)]) -> Result<u64, StoreError> {
        let recs = self.records(self.snapshot(None));
        let names = Self::names(&recs);
        let mut alloc = Allocator::new(&names);

        let tx = self.engine.with_write_txn(|mut txn| {
            let type_id = alloc.type_id(kind, &mut txn);
            let mut entity_props = Vec::with_capacity(props.len());
            for (name, value) in props {
                let pid = alloc.prop_id(name, &mut txn);
                entity_props.push((pid, value.clone()));
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
    pub fn set(&self, id: Uuid, property: &str, value: &Value) -> Result<u64, StoreError> {
        let snap = self.snapshot(None);
        let recs = self.records(snap);
        let names = Self::names(&recs);
        let mut alloc = Allocator::new(&names);

        let Resolved::Live(Record::Entity(current)) = self.engine.snapshot_read(&id, snap)? else {
            return Err(StoreError::NotFound);
        };

        let tx = self.engine.with_write_txn(|mut txn| {
            let pid = alloc.prop_id(property, &mut txn);
            let mut props = current.properties.clone();
            match props.iter_mut().find(|(p, _)| *p == pid) {
                Some(slot) => slot.1 = value.clone(),
                None => props.push((pid, value.clone())),
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
            .create("Person", &[("name".into(), s("Alice")), ("age".into(), Value::I64(30))])
            .expect("create alice");
        store
            .create("Person", &[("name".into(), s("Bob")), ("age".into(), Value::I64(25))])
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
        let table = store.table(tid, None, 1000);
        assert_eq!(table["total"], 2);
        assert_eq!(table["rows"].as_array().unwrap().len(), 2);
    }

    /// `set` commits a new version; the old value is still readable as-of an
    /// earlier transaction, on both the table and single-record paths.
    #[test]
    fn edit_creates_version_and_time_travel() {
        let store = fresh();
        let tx1 = store
            .create("Person", &[("age".into(), Value::I64(30))])
            .expect("create");
        let tid = u32::try_from(store.catalog(None)["kinds"][0]["type_id"].as_u64().unwrap()).unwrap();
        let id_str = store.table(tid, None, 10)["rows"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let id = Uuid::parse_str(&id_str).unwrap();

        store.set(id, "age", &Value::I64(31)).expect("set");

        // Now: 31. As of the create tx: 30.
        let now = store.record(id, None);
        assert_eq!(now["properties"][0]["value"], 31);
        let past = store.record(id, Some(tx1));
        assert_eq!(past["properties"][0]["value"], 30);

        // Table path honours the same snapshot.
        let past_table = store.table(tid, Some(tx1), 10);
        assert_eq!(past_table["rows"][0]["cells"][0], 30);
    }

    /// `delete` tombstones at head but the record remains visible in history.
    #[test]
    fn delete_tombstones_but_history_remains() {
        let store = fresh();
        let tx_create = store
            .create("Note", &[("body".into(), s("hi"))])
            .expect("create");
        let tid = u32::try_from(store.catalog(None)["kinds"][0]["type_id"].as_u64().unwrap()).unwrap();
        let id = Uuid::parse_str(store.table(tid, None, 10)["rows"][0]["id"].as_str().unwrap())
            .unwrap();

        store.delete(id).expect("delete");

        assert_eq!(store.table(tid, None, 10)["total"], 0, "gone at head");
        assert_eq!(
            store.table(tid, Some(tx_create), 10)["total"],
            1,
            "still in history"
        );
        assert!(store.record(id, None).is_null(), "no live record at head");
        assert!(!store.record(id, Some(tx_create)).is_null(), "live in history");
    }

    /// Editing a record that does not exist is a typed `NotFound` (HTTP 404).
    #[test]
    fn set_unknown_record_is_not_found() {
        let store = fresh();
        let err = store
            .set(Uuid::now_v7(), "x", &Value::I64(1))
            .expect_err("must fail");
        assert!(matches!(err, StoreError::NotFound));
        assert_eq!(err.status(), 404);
    }
}
