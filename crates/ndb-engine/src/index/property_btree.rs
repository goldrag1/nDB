//! Schema-driven property B-tree index — `(TypeId, PropertyId, Value) →
//! Set<EntityId>` (§14.2 / §17.1).
#![allow(clippy::doc_markdown, clippy::many_single_char_names)]
//!
//! Sixth of the six mandatory v1 indexes. Where the lookup-key reverse
//! index supports point lookups ("find the entity with email
//! alice@example.com"), this index supports both **point** and
//! **range** queries scoped to a type:
//!
//! - `find(type=Customer, prop=age, value=30) → {entity_ids}`
//! - `range(type=Customer, prop=age, 18..=65) → {entity_ids}`
//!
//! Implementation is a single `BTreeMap` keyed by `(TypeId, PropertyId,
//! ValueBytes)`. The same canonical-bytes representation used by the
//! lookup-key index is shared here (defined in this module's
//! `value_to_index_bytes` — same shape, lifted to a reusable helper).
//!
//! v1 model: in-memory, rebuilt on open, updated on commit, tombstone
//! removes, re-assertion replaces. Same out-of-order-replay handling as
//! the other indexes via per-entity `latest_tx` watermark.
//!
//! Selectivity hint: an entity carries its `type_id` once but possibly
//! many properties. This index emits one entry per (entity, indexed
//! property) for entities of the registered type. If the caller
//! registers a high-cardinality property (e.g. `description`), expect
//! lots of single-entity buckets — the data structure handles it but
//! range scans over such columns aren't useful. Recommended for
//! low-to-medium-cardinality numeric, timestamp, and short-string
//! columns.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::id::{EntityId, PropertyId, TxId, TypeId};
use crate::index::Index;
use crate::record::Record;
use crate::value::Value;

/// In-memory property B-tree index.
#[derive(Debug, Default)]
pub struct PropertyBTreeIndex {
    /// Registered `(type_id, property_id)` pairs the caller wants
    /// indexed.
    registered: HashSet<(TypeId, PropertyId)>,
    /// `(TypeId, PropertyId, ValueBytes) → entity ids with that value`.
    forward: BTreeMap<(TypeId, PropertyId, Vec<u8>), BTreeSet<EntityId>>,
    /// For each entity, the keys it currently owns in `forward` (so we
    /// can remove them efficiently on tombstone / re-assertion).
    by_entity: HashMap<EntityId, Vec<(TypeId, PropertyId, Vec<u8>)>>,
    /// Out-of-order detection.
    latest_tx: HashMap<EntityId, TxId>,
}

impl PropertyBTreeIndex {
    /// Empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare `(type_id, property_id)` as a B-tree-indexed column.
    /// Already-committed entities are NOT retroactively indexed; call
    /// `Engine::rebuild_indexes` after late registration.
    pub fn register(&mut self, type_id: TypeId, property_id: PropertyId) {
        self.registered.insert((type_id, property_id));
    }

    /// Whether a `(type, prop)` pair is registered.
    #[must_use]
    pub fn is_registered(&self, type_id: TypeId, property_id: PropertyId) -> bool {
        self.registered.contains(&(type_id, property_id))
    }

    /// Number of distinct `(type, prop, value)` keys currently indexed.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.forward.len()
    }

    /// True iff no keys are indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Point lookup: every entity of `type_id` whose `property_id`
    /// equals `value`.
    #[must_use]
    pub fn find(&self, type_id: TypeId, property_id: PropertyId, value: &Value) -> Vec<EntityId> {
        let Some(bytes) = value_to_index_bytes(value) else {
            return Vec::new();
        };
        self.forward
            .get(&(type_id, property_id, bytes))
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Range query: every entity of `type_id` whose `property_id` value
    /// falls in `[low, high]` (both inclusive). `None` for either bound
    /// is "unbounded on that side". Iteration order is ascending by
    /// canonical value bytes.
    #[must_use]
    pub fn range(
        &self,
        type_id: TypeId,
        property_id: PropertyId,
        low: Option<&Value>,
        high: Option<&Value>,
    ) -> Vec<EntityId> {
        let low_bytes = low.and_then(value_to_index_bytes);
        let high_bytes = high.and_then(value_to_index_bytes);
        let mut out = Vec::new();
        let start = match &low_bytes {
            Some(b) => std::ops::Bound::Included((type_id, property_id, b.clone())),
            None => std::ops::Bound::Included((type_id, property_id, Vec::new())),
        };
        // The natural upper bound for a (type, prop) bucket is the next
        // (type, prop+1) tuple — Rust's BTreeMap range needs a sentinel
        // we can construct; use the high value if supplied, else infer
        // a bucket-end.
        let end = match &high_bytes {
            Some(b) => std::ops::Bound::Included((type_id, property_id, b.clone())),
            None => {
                // Upper sentinel: same type, property_id + 1, empty
                // bytes. Since PropertyId is u32, only fails when
                // property_id is u32::MAX (extremely unlikely in v1);
                // in that case fall through to Unbounded.
                if property_id.get() == u32::MAX {
                    std::ops::Bound::Unbounded
                } else {
                    std::ops::Bound::Excluded((
                        type_id,
                        PropertyId::new(property_id.get() + 1),
                        Vec::new(),
                    ))
                }
            }
        };
        for (_, set) in self.forward.range((start, end)) {
            out.extend(set.iter().copied());
        }
        out
    }

    /// Top-`k` entities of `type_id` by `property_id`, **highest value
    /// first**. Walks only the tail of the bucket and stops at `k`
    /// (O(k + log N)), never materialising the whole column — the generic
    /// ordered-top-K primitive an application can use for "most X" without
    /// holding its own sorted list. Values must be of an order-preserving
    /// kind (numeric / timestamp), as for `range`.
    #[must_use]
    pub fn top_k(&self, type_id: TypeId, property_id: PropertyId, k: usize) -> Vec<EntityId> {
        if k == 0 {
            return Vec::new();
        }
        let start = std::ops::Bound::Included((type_id, property_id, Vec::new()));
        let end = if property_id.get() == u32::MAX {
            std::ops::Bound::Unbounded
        } else {
            std::ops::Bound::Excluded((type_id, PropertyId::new(property_id.get() + 1), Vec::new()))
        };
        let mut out = Vec::with_capacity(k);
        for (_, set) in self.forward.range((start, end)).rev() {
            for e in set {
                out.push(*e);
                if out.len() >= k {
                    return out;
                }
            }
        }
        out
    }

    fn remove_entries_for(&mut self, entity: &EntityId) {
        if let Some(keys) = self.by_entity.remove(entity) {
            for key in keys {
                if let Some(set) = self.forward.get_mut(&key) {
                    set.remove(entity);
                    if set.is_empty() {
                        self.forward.remove(&key);
                    }
                }
            }
        }
    }

    fn apply_entity(&mut self, e: &crate::record::EntityRecord) {
        let eid = e.entity_id;
        if let Some(prev) = self.latest_tx.get(&eid)
            && *prev > e.tx_id_assert
        {
            return;
        }
        self.remove_entries_for(&eid);
        let mut keys = Vec::new();
        for (prop, value) in &e.properties {
            if !self.registered.contains(&(e.type_id, *prop)) {
                continue;
            }
            if let Some(bytes) = value_to_index_bytes(value) {
                let key = (e.type_id, *prop, bytes);
                self.forward.entry(key.clone()).or_default().insert(eid);
                keys.push(key);
            }
        }
        if !keys.is_empty() {
            self.by_entity.insert(eid, keys);
        }
        self.latest_tx.insert(eid, e.tx_id_assert);
    }

    fn apply_tombstone(&mut self, t: &crate::record::TombstoneRecord) {
        let eid = EntityId::from_uuid(t.target_id);
        if let Some(prev) = self.latest_tx.get(&eid)
            && *prev > t.tx_id_supersede
        {
            return;
        }
        self.remove_entries_for(&eid);
        self.latest_tx.insert(eid, t.tx_id_supersede);
    }
}

impl Index for PropertyBTreeIndex {
    fn apply(&mut self, record: &Record, _tx_id: TxId) {
        match record {
            Record::Entity(e) => self.apply_entity(e),
            Record::Tombstone(t) => self.apply_tombstone(t),
            _ => {}
        }
    }

    fn clear(&mut self) {
        // Preserve registrations (they're metadata).
        self.forward.clear();
        self.by_entity.clear();
        self.latest_tx.clear();
    }

    fn name(&self) -> &'static str {
        "property-btree"
    }
}

/// Canonical bytes for a `Value` used by this index AND the lookup-key
/// index. Big-endian numeric types so byte-compare matches numeric
/// compare. Vectors / Extension / Null aren't indexable.
#[allow(clippy::match_same_arms)] // Per-variant arms kept for diff stability.
fn value_to_index_bytes(v: &Value) -> Option<Vec<u8>> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => vec![u8::from(*b)],
        Value::I64(n) => {
            // Flip sign bit so negative numbers sort before positives.
            let key = (n.cast_unsigned()) ^ (1u64 << 63);
            key.to_be_bytes().to_vec()
        }
        Value::F64(f) => {
            // IEEE-754 trick: flip sign bit on positives, flip all bits
            // on negatives, so byte-compare matches numeric order.
            let bits = f.to_bits();
            let key = if bits & (1u64 << 63) == 0 {
                bits ^ (1u64 << 63)
            } else {
                !bits
            };
            key.to_be_bytes().to_vec()
        }
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Bytes(b) => b.clone(),
        Value::Timestamp(t) => {
            let key = (t.cast_unsigned()) ^ (1u64 << 63);
            key.to_be_bytes().to_vec()
        }
        Value::EntityRef(id) => id.as_bytes().to_vec(),
        Value::Decimal { scale, mantissa } => {
            // Decimals only compare meaningfully at the same scale.
            // Embed scale in the prefix so different-scale buckets
            // don't accidentally collide.
            let mut out = Vec::with_capacity(17);
            out.push(*scale);
            // Mantissa flipped-sign-bit like I64.
            let key = (mantissa.cast_unsigned()) ^ (1u128 << 127);
            out.extend_from_slice(&key.to_be_bytes());
            out
        }
        Value::Vector(_) | Value::Extension(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::EntityRecord;

    fn entity(eid: EntityId, type_id: u32, tx: u64, props: Vec<(u32, Value)>) -> Record {
        Record::Entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(tx),
            tx_id_supersede: TxId::ACTIVE,
            properties: props
                .into_iter()
                .map(|(p, v)| (PropertyId::new(p), v))
                .collect(),
        })
    }

    #[test]
    fn register_then_find_exact() {
        let mut idx = PropertyBTreeIndex::new();
        let cust = TypeId::new(1);
        let age = PropertyId::new(10);
        idx.register(cust, age);
        let a = EntityId::now_v7();
        let b = EntityId::now_v7();
        idx.apply(&entity(a, 1, 1, vec![(10, Value::I64(30))]), TxId::new(1));
        idx.apply(&entity(b, 1, 2, vec![(10, Value::I64(40))]), TxId::new(2));
        let hits = idx.find(cust, age, &Value::I64(30));
        assert_eq!(hits, vec![a]);
        let none = idx.find(cust, age, &Value::I64(99));
        assert!(none.is_empty());
    }

    #[test]
    fn top_k_returns_highest_values_first() {
        let mut idx = PropertyBTreeIndex::new();
        let cust = TypeId::new(1);
        let cit = PropertyId::new(10);
        idx.register(cust, cit);
        let mut by_val = std::collections::HashMap::new();
        for v in [5_i64, 100, 30, 999, 7, 250] {
            let id = EntityId::now_v7();
            by_val.insert(id, v);
            idx.apply(&entity(id, 1, v as u64, vec![(10, Value::I64(v))]), TxId::new(v as u64));
        }
        // top 3 by citations → 999, 250, 100 (descending)
        let top = idx.top_k(cust, cit, 3);
        let vals: Vec<i64> = top.iter().map(|id| by_val[id]).collect();
        assert_eq!(vals, vec![999, 250, 100]);
        // k larger than the column returns all, still descending
        let all = idx.top_k(cust, cit, 100);
        assert_eq!(all.len(), 6);
        assert_eq!(by_val[&all[0]], 999);
        assert_eq!(by_val[&all[5]], 5);
        assert!(idx.top_k(cust, cit, 0).is_empty());
    }

    #[test]
    fn range_inclusive_bounds() {
        let mut idx = PropertyBTreeIndex::new();
        let cust = TypeId::new(1);
        let age = PropertyId::new(10);
        idx.register(cust, age);
        let mut ids = Vec::new();
        for v in [10, 20, 30, 40, 50] {
            let id = EntityId::now_v7();
            ids.push((id, v));
            idx.apply(
                &entity(
                    id,
                    1,
                    u64::try_from(v).unwrap(),
                    vec![(10, Value::I64(v.into()))],
                ),
                TxId::new(u64::try_from(v).unwrap()),
            );
        }
        let mut got = idx.range(cust, age, Some(&Value::I64(20)), Some(&Value::I64(40)));
        got.sort();
        let mut want: Vec<EntityId> = ids
            .iter()
            .filter(|(_, v)| (20..=40).contains(v))
            .map(|(id, _)| *id)
            .collect();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn range_unbounded_low_and_high() {
        let mut idx = PropertyBTreeIndex::new();
        let t = TypeId::new(1);
        let p = PropertyId::new(10);
        idx.register(t, p);
        let mut ids = Vec::new();
        for v in [-5_i64, 0, 5, 10] {
            let id = EntityId::now_v7();
            ids.push(id);
            let tx = u64::try_from(v + 100).unwrap();
            idx.apply(&entity(id, 1, tx, vec![(10, Value::I64(v))]), TxId::new(tx));
        }
        let all = idx.range(t, p, None, None);
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn negative_sorts_before_positive() {
        let mut idx = PropertyBTreeIndex::new();
        let t = TypeId::new(1);
        let p = PropertyId::new(10);
        idx.register(t, p);
        let neg = EntityId::now_v7();
        let pos = EntityId::now_v7();
        idx.apply(
            &entity(neg, 1, 1, vec![(10, Value::I64(-10))]),
            TxId::new(1),
        );
        idx.apply(&entity(pos, 1, 2, vec![(10, Value::I64(10))]), TxId::new(2));
        // Range [-100, 0] should include neg, exclude pos.
        let in_neg = idx.range(t, p, Some(&Value::I64(-100)), Some(&Value::I64(0)));
        assert_eq!(in_neg, vec![neg]);
        let in_pos = idx.range(t, p, Some(&Value::I64(1)), Some(&Value::I64(100)));
        assert_eq!(in_pos, vec![pos]);
    }

    #[test]
    fn reassertion_replaces_value() {
        let mut idx = PropertyBTreeIndex::new();
        let t = TypeId::new(1);
        let p = PropertyId::new(10);
        idx.register(t, p);
        let id = EntityId::now_v7();
        idx.apply(&entity(id, 1, 1, vec![(10, Value::I64(30))]), TxId::new(1));
        idx.apply(&entity(id, 1, 2, vec![(10, Value::I64(40))]), TxId::new(2));
        assert!(idx.find(t, p, &Value::I64(30)).is_empty());
        assert_eq!(idx.find(t, p, &Value::I64(40)), vec![id]);
    }

    #[test]
    fn tombstone_removes_entity() {
        let mut idx = PropertyBTreeIndex::new();
        let t = TypeId::new(1);
        let p = PropertyId::new(10);
        idx.register(t, p);
        let id = EntityId::now_v7();
        idx.apply(&entity(id, 1, 1, vec![(10, Value::I64(7))]), TxId::new(1));
        idx.apply(
            &Record::Tombstone(crate::record::TombstoneRecord {
                target_id: id.into_uuid(),
                tx_id_supersede: TxId::new(2),
            }),
            TxId::new(2),
        );
        assert!(idx.find(t, p, &Value::I64(7)).is_empty());
    }

    #[test]
    fn unregistered_pair_ignored() {
        let mut idx = PropertyBTreeIndex::new();
        // Don't register.
        let id = EntityId::now_v7();
        idx.apply(&entity(id, 1, 1, vec![(10, Value::I64(7))]), TxId::new(1));
        assert!(idx.is_empty());
    }

    #[test]
    fn string_range_lexicographic() {
        let mut idx = PropertyBTreeIndex::new();
        let t = TypeId::new(1);
        let p = PropertyId::new(10);
        idx.register(t, p);
        let mut ids = std::collections::HashMap::new();
        for name in ["alice", "bob", "carol", "dave"] {
            let id = EntityId::now_v7();
            ids.insert(name.to_string(), id);
            let tx = u64::try_from(name.len()).unwrap();
            idx.apply(
                &entity(id, 1, tx, vec![(10, Value::String(name.into()))]),
                TxId::new(tx),
            );
        }
        let mut got = idx.range(
            t,
            p,
            Some(&Value::String("b".into())),
            Some(&Value::String("c".into())),
        );
        got.sort();
        let mut want = vec![ids["bob"]];
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn multiple_entities_at_same_value() {
        let mut idx = PropertyBTreeIndex::new();
        let t = TypeId::new(1);
        let p = PropertyId::new(10);
        idx.register(t, p);
        let a = EntityId::now_v7();
        let b = EntityId::now_v7();
        let c = EntityId::now_v7();
        for (eid, tx) in [(a, 1), (b, 2), (c, 3)] {
            idx.apply(
                &entity(eid, 1, tx, vec![(10, Value::I64(42))]),
                TxId::new(tx),
            );
        }
        let mut got = idx.find(t, p, &Value::I64(42));
        got.sort();
        let mut want = vec![a, b, c];
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn out_of_order_older_assertion_ignored() {
        let mut idx = PropertyBTreeIndex::new();
        let t = TypeId::new(1);
        let p = PropertyId::new(10);
        idx.register(t, p);
        let id = EntityId::now_v7();
        idx.apply(&entity(id, 1, 5, vec![(10, Value::I64(99))]), TxId::new(5));
        idx.apply(&entity(id, 1, 1, vec![(10, Value::I64(1))]), TxId::new(1));
        assert!(idx.find(t, p, &Value::I64(1)).is_empty());
        assert_eq!(idx.find(t, p, &Value::I64(99)), vec![id]);
    }
}
