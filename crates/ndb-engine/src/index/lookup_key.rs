//! Lookup-key reverse index — `(property_id, value bytes) → entity_id` (§8.1).
#![allow(clippy::doc_markdown)]
//!
//! External lookup keys (`customer_code`, `email`, `tax_id`, ...) are the
//! human-readable handles applications use to find entities. Internal
//! storage is by UUID; this index makes the inverse direction O(log N).
//!
//! Which property IDs are "lookup keys" is the caller's choice — they
//! declare via [`LookupKeyIndex::register_property`]. The eventual v2
//! design will read this list from metadata hyperedges; v1 keeps it
//! in-memory and requires re-registration on each `Engine::open`.
//!
//! What we index:
//!
//! - Every value of every property in `registered_properties` on every
//!   live (non-superseded) entity record gets an entry.
//! - When a tombstone arrives for an entity uuid, all entries for that
//!   uuid across registered properties are removed.
//! - When an entity is RE-asserted with a different value for a registered
//!   property, the OLD entry is removed and the new one inserted. Since
//!   v1 derives supersession at read time (mvcc.rs), the index tracks
//!   the latest-asserted value per (uuid, property_id).

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::id::{EntityId, PropertyId, TxId};
use crate::index::Index;
use crate::record::Record;
use crate::value::Value;

/// In-memory lookup-key reverse index. Thread-unsafe by design — wrap in
/// an external mutex if needed.
#[derive(Debug, Default)]
pub struct LookupKeyIndex {
    /// Set of property IDs the caller declared as lookup keys.
    registered_properties: HashSet<PropertyId>,
    /// `(property_id, value_bytes) → entity_id`. `BTreeMap` so equal keys
    /// dedupe naturally and prefix scans can join later.
    forward: BTreeMap<(PropertyId, Vec<u8>), EntityId>,
    /// For each entity, the (property_id, value_bytes) tuples it currently
    /// owns. Used to remove old entries on re-assertion and tombstone.
    by_entity: HashMap<EntityId, Vec<(PropertyId, Vec<u8>)>>,
    /// For each entity, the latest `tx_id_assert` we've processed. Newer
    /// assertions overwrite older index entries; older assertions arriving
    /// later (e.g. out-of-order replay during recovery) are ignored.
    latest_tx: HashMap<EntityId, TxId>,
}

impl LookupKeyIndex {
    /// New, empty index with no registered properties.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rough estimate of resident heap bytes (diagnostic; walks the maps).
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        const OVH: usize = 32;
        let mut n = self.registered_properties.len() * (4 + OVH);
        for k in self.forward.keys() {
            n += 4 + k.1.len() + 16 + OVH; // (PropertyId, value bytes) -> EntityId
        }
        for keys in self.by_entity.values() {
            n += 16 + 24 + OVH;
            for k in keys {
                n += 4 + k.1.len() + 16;
            }
        }
        n += self.latest_tx.len() * (16 + 8 + OVH);
        n
    }

    /// Declare a property as a lookup key. Idempotent. Must be called
    /// BEFORE the first record using this property is applied; otherwise
    /// the historical record won't be indexed (re-open of the engine
    /// would pick it up after re-registration).
    pub fn register_property(&mut self, property_id: PropertyId) {
        self.registered_properties.insert(property_id);
    }

    /// True iff `property_id` is registered as a lookup key.
    #[must_use]
    pub fn is_registered(&self, property_id: PropertyId) -> bool {
        self.registered_properties.contains(&property_id)
    }

    /// True iff any lookup-key property is registered.
    #[must_use]
    pub fn has_registrations(&self) -> bool {
        !self.registered_properties.is_empty()
    }

    /// Look up an entity by `(property_id, value)`. Returns `None` if no
    /// live entity owns this pair.
    #[must_use]
    pub fn lookup(&self, property_id: PropertyId, value: &Value) -> Option<EntityId> {
        let key = (property_id, value_to_index_bytes(value)?);
        self.forward.get(&key).copied()
    }

    /// Number of distinct `(property_id, value)` entries currently indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Properties currently registered as lookup keys.
    pub fn registered_properties(&self) -> impl Iterator<Item = PropertyId> + '_ {
        self.registered_properties.iter().copied()
    }

    fn remove_entries_for(&mut self, entity: &EntityId) {
        if let Some(entries) = self.by_entity.remove(entity) {
            for entry in entries {
                if self.forward.get(&entry) == Some(entity) {
                    self.forward.remove(&entry);
                }
            }
        }
    }

    fn apply_entity(&mut self, record: &crate::record::EntityRecord) {
        // Out-of-order arrivals (older `tx_id_assert` showing up after a
        // newer one) are ignored — the index always reflects the latest
        // assertion we've seen for an entity.
        let entity_id = record.entity_id;
        if let Some(prev) = self.latest_tx.get(&entity_id)
            && *prev > record.tx_id_assert
        {
            return;
        }
        self.remove_entries_for(&entity_id);
        let mut entries = Vec::new();
        for (prop_id, value) in &record.properties {
            if !self.registered_properties.contains(prop_id) {
                continue;
            }
            if let Some(bytes) = value_to_index_bytes(value) {
                let key = (*prop_id, bytes);
                self.forward.insert(key.clone(), entity_id);
                entries.push(key);
            }
        }
        if !entries.is_empty() {
            self.by_entity.insert(entity_id, entries);
        }
        self.latest_tx.insert(entity_id, record.tx_id_assert);
    }

    fn apply_tombstone(&mut self, record: &crate::record::TombstoneRecord) {
        // Tombstone uuid could refer to an entity or a hyperedge; we only
        // care if it's an entity in our by_entity map.
        let target = EntityId::from_uuid(record.target_id);
        if let Some(prev) = self.latest_tx.get(&target)
            && *prev > record.tx_id_supersede
        {
            return;
        }
        self.remove_entries_for(&target);
        self.latest_tx.insert(target, record.tx_id_supersede);
    }
}

impl Index for LookupKeyIndex {
    fn apply(&mut self, record: &Record, _tx_id: TxId) {
        match record {
            Record::Entity(e) => self.apply_entity(e),
            Record::Tombstone(t) => self.apply_tombstone(t),
            // Hyperedges, dictionary records: not relevant to this index.
            _ => {}
        }
    }

    fn clear(&mut self) {
        // Preserve registered properties — they're metadata, not data.
        self.forward.clear();
        self.by_entity.clear();
        self.latest_tx.clear();
    }

    fn name(&self) -> &'static str {
        "lookup-key"
    }
}

/// Convert a `Value` to canonical bytes for use as an index key.
///
/// Only "atomic-shaped" values are indexable. Vectors, decimals, raw
/// bytes are allowed; null is not (can't form a useful lookup key).
/// Extension values are deliberately excluded — the build doesn't know
/// their semantics.
#[allow(clippy::match_same_arms)] // Per-variant arms kept for diff stability when adding new tags.
pub(crate) fn value_to_index_bytes(v: &Value) -> Option<Vec<u8>> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => vec![u8::from(*b)],
        Value::I64(n) => n.to_be_bytes().to_vec(),
        Value::F64(f) => f.to_bits().to_be_bytes().to_vec(),
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Bytes(b) => b.clone(),
        Value::Timestamp(t) => t.to_be_bytes().to_vec(),
        Value::EntityRef(id) => id.as_bytes().to_vec(),
        Value::Decimal { scale, mantissa } => {
            let mut out = Vec::with_capacity(17);
            out.push(*scale);
            out.extend_from_slice(&mantissa.to_be_bytes());
            out
        }
        Value::Vector(v) => {
            let mut out = Vec::with_capacity(4 * v.len());
            for f in v {
                out.extend_from_slice(&f.to_bits().to_be_bytes());
            }
            out
        }
        Value::Extension(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::TypeId;
    use crate::record::EntityRecord;

    fn entity(eid: EntityId, tx: u64, props: Vec<(u32, Value)>) -> Record {
        Record::Entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(tx),
            tx_id_supersede: TxId::ACTIVE,
            properties: props
                .into_iter()
                .map(|(p, v)| (PropertyId::new(p), v))
                .collect(),
        })
    }

    #[test]
    fn register_then_apply_then_lookup() {
        let mut idx = LookupKeyIndex::new();
        idx.register_property(PropertyId::new(1)); // "email"
        let eid = EntityId::now_v7();
        idx.apply(
            &entity(
                eid,
                10,
                vec![(1, Value::String("alice@example.com".into()))],
            ),
            TxId::new(10),
        );
        let hit = idx.lookup(
            PropertyId::new(1),
            &Value::String("alice@example.com".into()),
        );
        assert_eq!(hit, Some(eid));
        // Miss on unregistered value.
        assert!(
            idx.lookup(PropertyId::new(1), &Value::String("bob@example.com".into()))
                .is_none()
        );
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn unregistered_property_ignored() {
        let mut idx = LookupKeyIndex::new();
        // Don't register property 1.
        let eid = EntityId::now_v7();
        idx.apply(
            &entity(eid, 1, vec![(1, Value::String("ignored".into()))]),
            TxId::new(1),
        );
        assert!(idx.is_empty());
    }

    #[test]
    fn reassertion_overwrites_value() {
        let mut idx = LookupKeyIndex::new();
        idx.register_property(PropertyId::new(1));
        let eid = EntityId::now_v7();
        idx.apply(
            &entity(eid, 10, vec![(1, Value::String("old".into()))]),
            TxId::new(10),
        );
        idx.apply(
            &entity(eid, 20, vec![(1, Value::String("new".into()))]),
            TxId::new(20),
        );
        assert!(
            idx.lookup(PropertyId::new(1), &Value::String("old".into()))
                .is_none()
        );
        assert_eq!(
            idx.lookup(PropertyId::new(1), &Value::String("new".into())),
            Some(eid)
        );
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn tombstone_removes_entries() {
        let mut idx = LookupKeyIndex::new();
        idx.register_property(PropertyId::new(1));
        let eid = EntityId::now_v7();
        idx.apply(
            &entity(eid, 10, vec![(1, Value::String("alice".into()))]),
            TxId::new(10),
        );
        idx.apply(
            &Record::Tombstone(crate::record::TombstoneRecord {
                target_id: eid.into_uuid(),
                tx_id_supersede: TxId::new(20),
            }),
            TxId::new(20),
        );
        assert!(
            idx.lookup(PropertyId::new(1), &Value::String("alice".into()))
                .is_none()
        );
    }

    #[test]
    fn out_of_order_older_assertion_ignored() {
        let mut idx = LookupKeyIndex::new();
        idx.register_property(PropertyId::new(1));
        let eid = EntityId::now_v7();
        // First apply the newer assertion (tx=20).
        idx.apply(
            &entity(eid, 20, vec![(1, Value::String("new".into()))]),
            TxId::new(20),
        );
        // Then an older one (tx=10) — should be ignored.
        idx.apply(
            &entity(eid, 10, vec![(1, Value::String("old".into()))]),
            TxId::new(10),
        );
        assert_eq!(
            idx.lookup(PropertyId::new(1), &Value::String("new".into())),
            Some(eid)
        );
        assert!(
            idx.lookup(PropertyId::new(1), &Value::String("old".into()))
                .is_none()
        );
    }

    #[test]
    fn multiple_lookup_keys_per_entity() {
        let mut idx = LookupKeyIndex::new();
        idx.register_property(PropertyId::new(1)); // email
        idx.register_property(PropertyId::new(2)); // tax_id
        let eid = EntityId::now_v7();
        idx.apply(
            &entity(
                eid,
                5,
                vec![
                    (1, Value::String("alice@x.com".into())),
                    (2, Value::String("VN-123".into())),
                ],
            ),
            TxId::new(5),
        );
        assert_eq!(idx.len(), 2);
        assert_eq!(
            idx.lookup(PropertyId::new(1), &Value::String("alice@x.com".into())),
            Some(eid)
        );
        assert_eq!(
            idx.lookup(PropertyId::new(2), &Value::String("VN-123".into())),
            Some(eid)
        );
    }

    #[test]
    fn clear_resets_data_but_keeps_registrations() {
        let mut idx = LookupKeyIndex::new();
        idx.register_property(PropertyId::new(1));
        idx.apply(
            &entity(EntityId::now_v7(), 1, vec![(1, Value::String("x".into()))]),
            TxId::new(1),
        );
        assert_eq!(idx.len(), 1);
        idx.clear();
        assert!(idx.is_empty());
        assert!(idx.is_registered(PropertyId::new(1)));
    }
}
