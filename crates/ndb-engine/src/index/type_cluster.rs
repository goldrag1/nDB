//! Hyperedge-type clustering index — `type_id → [hyperedge_ids]` (§14.2).
#![allow(clippy::doc_markdown)]
//!
//! Answers "give me every approval", "give me every transcription event".
//! Cheap compared to the adjacency index because it carries one entry per
//! hyperedge instead of one per `(hyperedge, role)` pair, but conceptually
//! the same pattern.
//!
//! Same v1 model as the other in-memory indexes: rebuilt on
//! `Engine::open`, updated on commit, tombstones remove entries, latest
//! `tx_id_assert` wins.

use std::collections::{BTreeSet, HashMap};

use crate::id::{HyperedgeId, TxId, TypeId};
use crate::index::Index;
use crate::record::Record;

/// In-memory hyperedge-type clustering index.
#[derive(Debug, Default)]
pub struct HyperEdgeTypeIndex {
    /// `type_id → set of hyperedge ids of that type`.
    forward: HashMap<TypeId, BTreeSet<HyperedgeId>>,
    /// For each hyperedge, the type it was last asserted with. Used to
    /// move it between buckets when re-asserted with a different type.
    by_hyperedge: HashMap<HyperedgeId, TypeId>,
    /// Out-of-order detection.
    latest_tx: HashMap<HyperedgeId, TxId>,
}

impl HyperEdgeTypeIndex {
    /// New, empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// All hyperedges of a given type, in ascending hyperedge-id order.
    pub fn by_type(&self, type_id: TypeId) -> impl Iterator<Item = HyperedgeId> + '_ {
        self.forward
            .get(&type_id)
            .into_iter()
            .flat_map(|set| set.iter().copied())
    }

    /// Convenience: collect [`by_type`](Self::by_type) into a `Vec`.
    #[must_use]
    pub fn by_type_vec(&self, type_id: TypeId) -> Vec<HyperedgeId> {
        self.by_type(type_id).collect()
    }

    /// Cardinality of a type bucket.
    #[must_use]
    pub fn count(&self, type_id: TypeId) -> usize {
        self.forward.get(&type_id).map_or(0, BTreeSet::len)
    }

    /// Whether `hid` is currently clustered under `type_id`. O(1) —
    /// reads the reverse `by_hyperedge` map instead of materialising the
    /// whole type bucket. Equivalent to `by_type(type_id).contains(hid)`
    /// because `forward[t]` and `by_hyperedge[h]==t` are maintained in
    /// lock-step (see `apply_hyperedge` / `remove`).
    #[must_use]
    pub fn is_type(&self, type_id: TypeId, hid: HyperedgeId) -> bool {
        self.by_hyperedge.get(&hid) == Some(&type_id)
    }

    /// Number of distinct types currently observed.
    #[must_use]
    pub fn type_count(&self) -> usize {
        self.forward.len()
    }

    fn remove(&mut self, hid: &HyperedgeId) {
        if let Some(old_type) = self.by_hyperedge.remove(hid)
            && let Some(set) = self.forward.get_mut(&old_type)
        {
            set.remove(hid);
            if set.is_empty() {
                self.forward.remove(&old_type);
            }
        }
    }

    fn apply_hyperedge(&mut self, h: &crate::record::HyperEdgeRecord) {
        if let Some(prev) = self.latest_tx.get(&h.hyperedge_id)
            && *prev > h.tx_id_assert
        {
            return;
        }
        self.remove(&h.hyperedge_id);
        self.forward
            .entry(h.type_id)
            .or_default()
            .insert(h.hyperedge_id);
        self.by_hyperedge.insert(h.hyperedge_id, h.type_id);
        self.latest_tx.insert(h.hyperedge_id, h.tx_id_assert);
    }

    fn apply_tombstone(&mut self, t: &crate::record::TombstoneRecord) {
        let hid = HyperedgeId::from_uuid(t.target_id);
        if let Some(prev) = self.latest_tx.get(&hid)
            && *prev > t.tx_id_supersede
        {
            return;
        }
        self.remove(&hid);
        self.latest_tx.insert(hid, t.tx_id_supersede);
    }
}

impl Index for HyperEdgeTypeIndex {
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
        "hyperedge-type-cluster"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{EntityId, RoleId};
    use crate::record::HyperEdgeRecord;

    fn hyperedge(hid: HyperedgeId, type_id: u32, tx: u64) -> Record {
        Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: hid,
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(tx),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(1), EntityId::now_v7())],
            hyperedge_roles: Vec::new(),
            properties: vec![],
        })
    }

    #[test]
    fn group_by_type() {
        let mut idx = HyperEdgeTypeIndex::new();
        let h1 = HyperedgeId::now_v7();
        let h2 = HyperedgeId::now_v7();
        let h3 = HyperedgeId::now_v7();
        idx.apply(&hyperedge(h1, 1, 1), TxId::new(1));
        idx.apply(&hyperedge(h2, 1, 2), TxId::new(2));
        idx.apply(&hyperedge(h3, 2, 3), TxId::new(3));
        assert_eq!(idx.count(TypeId::new(1)), 2);
        assert_eq!(idx.count(TypeId::new(2)), 1);
        assert_eq!(idx.type_count(), 2);
        let mut t1 = idx.by_type_vec(TypeId::new(1));
        t1.sort();
        let mut want = vec![h1, h2];
        want.sort();
        assert_eq!(t1, want);
    }

    #[test]
    fn reassertion_with_new_type_moves_bucket() {
        let mut idx = HyperEdgeTypeIndex::new();
        let h = HyperedgeId::now_v7();
        idx.apply(&hyperedge(h, 1, 1), TxId::new(1));
        idx.apply(&hyperedge(h, 2, 2), TxId::new(2));
        assert_eq!(idx.count(TypeId::new(1)), 0);
        assert_eq!(idx.count(TypeId::new(2)), 1);
    }

    #[test]
    fn tombstone_removes_from_bucket() {
        let mut idx = HyperEdgeTypeIndex::new();
        let h = HyperedgeId::now_v7();
        idx.apply(&hyperedge(h, 1, 1), TxId::new(1));
        idx.apply(
            &Record::Tombstone(crate::record::TombstoneRecord {
                target_id: h.into_uuid(),
                tx_id_supersede: TxId::new(2),
            }),
            TxId::new(2),
        );
        assert_eq!(idx.count(TypeId::new(1)), 0);
    }
}
