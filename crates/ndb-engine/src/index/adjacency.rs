//! Adjacency index — `entity_id → [hyperedge_ids referencing it]` (§14.2).
#![allow(clippy::doc_markdown)]
//!
//! "Find all approvals where Alice is the approver" is THE canonical
//! traversal query in a hypergraph database, and it doesn't have a
//! useful answer without this index. A linear scan over every hyperedge
//! checking every role is O(N × arity); the adjacency index makes it
//! O(neighbors).
//!
//! v1 model:
//!
//! - In-memory, rebuilt on `Engine::open` from the primary store + WAL
//!   replay (same recovery flow as the lookup-key index).
//! - Every hyperedge contributes one entry per role: for each `(role_id,
//!   entity_id)` in `roles`, record `entity_id → hyperedge_id`.
//! - On tombstone for a hyperedge, every entry pointing at that
//!   hyperedge is removed.
//! - Out-of-order `tx_id_assert` arrivals (replay racing) are handled by
//!   keeping the latest set of role players per hyperedge — newer
//!   replaces older.
//!
//! What the index does NOT carry (v1):
//!
//! - **Role information.** Callers get hyperedge ids back; if they need
//!   to know "Alice's role in this hyperedge", they fetch the hyperedge
//!   record and inspect it. v2 may extend to `entity → [(hyperedge,
//!   role)]`.
//! - **Snapshot awareness.** All-time-latest semantics, same as
//!   `LookupKeyIndex`.

use std::collections::{BTreeSet, HashMap};

use crate::id::{EntityId, HyperedgeId, TxId};
use crate::index::Index;
use crate::record::Record;

/// In-memory adjacency index.
#[derive(Debug, Default)]
pub struct AdjacencyIndex {
    /// `entity_id → set of hyperedge ids that reference it in any role`.
    /// `BTreeSet` so iteration is deterministic + dedup is O(log N).
    forward: HashMap<EntityId, BTreeSet<HyperedgeId>>,
    /// For each hyperedge, the entities it currently references. Used to
    /// remove old entries on re-assertion or tombstone.
    by_hyperedge: HashMap<HyperedgeId, Vec<EntityId>>,
    /// Latest `tx_id_assert` seen per hyperedge, for out-of-order detection.
    latest_tx: HashMap<HyperedgeId, TxId>,
}

impl AdjacencyIndex {
    /// New, empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// All hyperedges currently referencing `entity`. Returns an empty
    /// iterator if the entity has no neighbors. Order is by hyperedge id
    /// ascending (BTreeSet iteration).
    pub fn neighbors(&self, entity: EntityId) -> impl Iterator<Item = HyperedgeId> + '_ {
        self.forward
            .get(&entity)
            .into_iter()
            .flat_map(|set| set.iter().copied())
    }

    /// Convenience: collect [`neighbors`](Self::neighbors) into a `Vec`.
    #[must_use]
    pub fn neighbors_vec(&self, entity: EntityId) -> Vec<HyperedgeId> {
        self.neighbors(entity).collect()
    }

    /// Degree of `entity` — number of hyperedges referencing it.
    #[must_use]
    pub fn degree(&self, entity: EntityId) -> usize {
        self.forward.get(&entity).map_or(0, BTreeSet::len)
    }

    /// Total number of distinct entities with at least one hyperedge.
    #[must_use]
    pub fn entity_count(&self) -> usize {
        self.forward.len()
    }

    /// Total number of distinct hyperedges currently indexed.
    #[must_use]
    pub fn hyperedge_count(&self) -> usize {
        self.by_hyperedge.len()
    }

    fn remove_entries_for(&mut self, hyperedge: &HyperedgeId) {
        if let Some(entities) = self.by_hyperedge.remove(hyperedge) {
            for entity in entities {
                if let Some(set) = self.forward.get_mut(&entity) {
                    set.remove(hyperedge);
                    if set.is_empty() {
                        self.forward.remove(&entity);
                    }
                }
            }
        }
    }

    fn apply_hyperedge(&mut self, record: &crate::record::HyperEdgeRecord) {
        let hid = record.hyperedge_id;
        if let Some(prev) = self.latest_tx.get(&hid)
            && *prev > record.tx_id_assert
        {
            return;
        }
        self.remove_entries_for(&hid);
        let mut entities = Vec::with_capacity(record.roles.len());
        for (_role_id, entity) in &record.roles {
            entities.push(*entity);
            self.forward.entry(*entity).or_default().insert(hid);
        }
        if !entities.is_empty() {
            self.by_hyperedge.insert(hid, entities);
        }
        self.latest_tx.insert(hid, record.tx_id_assert);
    }

    fn apply_tombstone(&mut self, record: &crate::record::TombstoneRecord) {
        let hid = HyperedgeId::from_uuid(record.target_id);
        if let Some(prev) = self.latest_tx.get(&hid)
            && *prev > record.tx_id_supersede
        {
            return;
        }
        self.remove_entries_for(&hid);
        self.latest_tx.insert(hid, record.tx_id_supersede);
    }
}

impl Index for AdjacencyIndex {
    fn apply(&mut self, record: &Record, _tx_id: TxId) {
        match record {
            Record::HyperEdge(h) => self.apply_hyperedge(h),
            Record::Tombstone(t) => self.apply_tombstone(t),
            _ => {}
        }
    }

    fn clear(&mut self) {
        self.forward.clear();
        self.by_hyperedge.clear();
        self.latest_tx.clear();
    }

    fn name(&self) -> &'static str {
        "adjacency"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{PropertyId, RoleId, TypeId};
    use crate::record::HyperEdgeRecord;

    fn hyperedge(hid: HyperedgeId, tx: u64, roles: Vec<(u32, EntityId)>) -> Record {
        Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: hid,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(tx),
            tx_id_supersede: TxId::ACTIVE,
            roles: roles
                .into_iter()
                .map(|(r, e)| (RoleId::new(r), e))
                .collect(),
            properties: vec![],
        })
    }

    #[test]
    fn neighbors_after_single_hyperedge() {
        let mut idx = AdjacencyIndex::new();
        let alice = EntityId::now_v7();
        let bob = EntityId::now_v7();
        let h = HyperedgeId::now_v7();
        idx.apply(&hyperedge(h, 1, vec![(1, alice), (2, bob)]), TxId::new(1));
        assert_eq!(idx.neighbors_vec(alice), vec![h]);
        assert_eq!(idx.neighbors_vec(bob), vec![h]);
        assert_eq!(idx.degree(alice), 1);
        assert_eq!(idx.degree(bob), 1);
    }

    #[test]
    fn many_hyperedges_share_an_entity() {
        let mut idx = AdjacencyIndex::new();
        let alice = EntityId::now_v7();
        let mut hids = Vec::new();
        for i in 0..5 {
            let h = HyperedgeId::now_v7();
            hids.push(h);
            idx.apply(
                &hyperedge(h, i + 1, vec![(1, alice), (2, EntityId::now_v7())]),
                TxId::new(i + 1),
            );
        }
        assert_eq!(idx.degree(alice), 5);
        let mut got = idx.neighbors_vec(alice);
        got.sort();
        hids.sort();
        assert_eq!(got, hids);
    }

    #[test]
    fn tombstone_removes_hyperedge_from_all_neighbors() {
        let mut idx = AdjacencyIndex::new();
        let alice = EntityId::now_v7();
        let bob = EntityId::now_v7();
        let h = HyperedgeId::now_v7();
        idx.apply(&hyperedge(h, 1, vec![(1, alice), (2, bob)]), TxId::new(1));
        idx.apply(
            &Record::Tombstone(crate::record::TombstoneRecord {
                target_id: h.into_uuid(),
                tx_id_supersede: TxId::new(2),
            }),
            TxId::new(2),
        );
        assert_eq!(idx.degree(alice), 0);
        assert_eq!(idx.degree(bob), 0);
        assert_eq!(idx.hyperedge_count(), 0);
    }

    #[test]
    fn reassertion_with_different_roles_swaps_neighbors() {
        let mut idx = AdjacencyIndex::new();
        let alice = EntityId::now_v7();
        let bob = EntityId::now_v7();
        let carol = EntityId::now_v7();
        let h = HyperedgeId::now_v7();
        idx.apply(&hyperedge(h, 1, vec![(1, alice), (2, bob)]), TxId::new(1));
        // Re-assert with carol instead of bob.
        idx.apply(&hyperedge(h, 2, vec![(1, alice), (2, carol)]), TxId::new(2));
        assert_eq!(idx.neighbors_vec(alice), vec![h]);
        assert_eq!(idx.degree(bob), 0);
        assert_eq!(idx.neighbors_vec(carol), vec![h]);
    }

    #[test]
    fn clear_drops_all_state() {
        let mut idx = AdjacencyIndex::new();
        let alice = EntityId::now_v7();
        let h = HyperedgeId::now_v7();
        idx.apply(&hyperedge(h, 1, vec![(1, alice)]), TxId::new(1));
        idx.clear();
        assert_eq!(idx.degree(alice), 0);
        assert_eq!(idx.hyperedge_count(), 0);
    }

    #[test]
    fn entity_records_and_property_keys_ignored() {
        let mut idx = AdjacencyIndex::new();
        idx.apply(
            &Record::Entity(crate::record::EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: TypeId::new(1),
                tx_id_assert: TxId::new(1),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![],
            }),
            TxId::new(1),
        );
        idx.apply(
            &Record::PropertyKey(crate::record::PropertyKeyRecord {
                id: PropertyId::new(1),
                name: "x".into(),
            }),
            TxId::new(2),
        );
        assert_eq!(idx.entity_count(), 0);
        assert_eq!(idx.hyperedge_count(), 0);
    }
}
