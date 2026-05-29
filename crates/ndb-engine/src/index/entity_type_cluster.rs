//! Entity-type clustering index — `type_id → [entity_ids]`.
#![allow(clippy::doc_markdown)]
//!
//! Mirror of [`HyperEdgeTypeIndex`](crate::index::HyperEdgeTypeIndex) for
//! entity records. Answers "give me every customer", "give me every
//! protein", and — load-bearing for the v3 count-aggregate fast path —
//! "how many entities of this type exist?" in O(1).
//!
//! Same v1 model as every other in-memory index: rebuilt on
//! `Engine::open`, updated on commit, tombstones remove entries, latest
//! `tx_id_assert` wins.

use std::collections::{BTreeSet, HashMap};

use crate::id::{EntityId, TxId, TypeId};
use crate::index::Index;
use crate::record::Record;

/// In-memory entity-type clustering index.
#[derive(Debug, Default)]
pub struct EntityTypeIndex {
    /// `type_id → set of entity ids of that type`.
    forward: HashMap<TypeId, BTreeSet<EntityId>>,
    /// For each entity, the type it was last asserted with. Used to
    /// move it between buckets when re-asserted with a different type.
    by_entity: HashMap<EntityId, TypeId>,
    /// Out-of-order detection.
    latest_tx: HashMap<EntityId, TxId>,
}

impl EntityTypeIndex {
    /// New, empty index.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Rough estimate of resident heap bytes (diagnostic; walks the maps).
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        const OVH: usize = 32;
        let mut n = 0usize;
        for set in self.forward.values() {
            n += 4 + 24 + OVH; // TypeId key + BTreeSet header
            n += set.len() * (16 + OVH); // EntityId per member
        }
        n += self.by_entity.len() * (16 + 4 + OVH);
        n += self.latest_tx.len() * (16 + 8 + OVH);
        n
    }

    /// All entities of a given type, in ascending entity-id order.
    pub fn by_type(&self, type_id: TypeId) -> impl Iterator<Item = EntityId> + '_ {
        self.forward.get(&type_id).into_iter().flat_map(|set| set.iter().copied())
    }

    /// Convenience: collect [`by_type`](Self::by_type) into a `Vec`.
    #[must_use]
    pub fn by_type_vec(&self, type_id: TypeId) -> Vec<EntityId> {
        self.by_type(type_id).collect()
    }

    /// Cardinality of a type bucket. O(1).
    #[must_use]
    pub fn count(&self, type_id: TypeId) -> usize {
        self.forward.get(&type_id).map_or(0, BTreeSet::len)
    }

    /// Number of distinct types currently observed.
    #[must_use]
    pub fn type_count(&self) -> usize { self.forward.len() }

    fn remove(&mut self, eid: &EntityId) {
        if let Some(old_type) = self.by_entity.remove(eid)
            && let Some(set) = self.forward.get_mut(&old_type)
        {
            set.remove(eid);
            if set.is_empty() {
                self.forward.remove(&old_type);
            }
        }
    }

    fn apply_entity(&mut self, e: &crate::record::EntityRecord) {
        if let Some(prev) = self.latest_tx.get(&e.entity_id)
            && *prev > e.tx_id_assert
        {
            return;
        }
        self.remove(&e.entity_id);
        self.forward.entry(e.type_id).or_default().insert(e.entity_id);
        self.by_entity.insert(e.entity_id, e.type_id);
        self.latest_tx.insert(e.entity_id, e.tx_id_assert);
    }

    fn apply_tombstone(&mut self, t: &crate::record::TombstoneRecord) {
        let eid = EntityId::from_uuid(t.target_id);
        if let Some(prev) = self.latest_tx.get(&eid)
            && *prev > t.tx_id_supersede
        {
            return;
        }
        self.remove(&eid);
        self.latest_tx.insert(eid, t.tx_id_supersede);
    }
}

impl Index for EntityTypeIndex {
    fn apply(&mut self, record: &Record, _tx_id: TxId) {
        match record {
            Record::Entity(e)   => self.apply_entity(e),
            Record::Tombstone(t) => self.apply_tombstone(t),
            _ => {}
        }
    }
    fn clear(&mut self) {
        self.forward.clear();
        self.by_entity.clear();
        self.latest_tx.clear();
    }
    fn name(&self) -> &'static str { "entity-type-cluster" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::TxId;
    use crate::record::EntityRecord;
    use crate::value::Value;
    use crate::id::PropertyId;

    fn entity(eid: EntityId, type_id: u32, tx: u64) -> Record {
        Record::Entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(tx),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(1), Value::I64(tx as i64))],
        })
    }

    #[test]
    fn group_by_type() {
        let mut idx = EntityTypeIndex::new();
        let alice = EntityId::now_v7();
        let bob = EntityId::now_v7();
        let carol = EntityId::now_v7();
        idx.apply(&entity(alice, 1, 1), TxId::new(1));
        idx.apply(&entity(bob, 1, 2), TxId::new(2));
        idx.apply(&entity(carol, 2, 3), TxId::new(3));

        let mut t1 = idx.by_type_vec(TypeId::new(1));
        t1.sort();
        let mut expected = vec![alice, bob];
        expected.sort();
        assert_eq!(t1, expected);
        assert_eq!(idx.by_type_vec(TypeId::new(2)), vec![carol]);
        assert_eq!(idx.count(TypeId::new(1)), 2);
        assert_eq!(idx.count(TypeId::new(2)), 1);
        assert_eq!(idx.count(TypeId::new(99)), 0);
    }

    #[test]
    fn re_assert_moves_between_types() {
        let mut idx = EntityTypeIndex::new();
        let alice = EntityId::now_v7();
        idx.apply(&entity(alice, 1, 1), TxId::new(1));
        idx.apply(&entity(alice, 2, 5), TxId::new(5));
        assert_eq!(idx.count(TypeId::new(1)), 0, "old bucket should be empty");
        assert_eq!(idx.count(TypeId::new(2)), 1);
    }

    #[test]
    fn out_of_order_assertion_skipped() {
        let mut idx = EntityTypeIndex::new();
        let alice = EntityId::now_v7();
        idx.apply(&entity(alice, 1, 5), TxId::new(5));
        // Older tx — should not move buckets.
        idx.apply(&entity(alice, 2, 1), TxId::new(1));
        assert_eq!(idx.count(TypeId::new(1)), 1);
        assert_eq!(idx.count(TypeId::new(2)), 0);
    }

    #[test]
    fn tombstone_removes_entry() {
        let mut idx = EntityTypeIndex::new();
        let alice = EntityId::now_v7();
        idx.apply(&entity(alice, 1, 1), TxId::new(1));
        idx.apply(&Record::Tombstone(crate::record::TombstoneRecord {
            target_id: alice.into_uuid(),
            tx_id_supersede: TxId::new(2),
        }), TxId::new(2));
        assert_eq!(idx.count(TypeId::new(1)), 0);
    }
}
