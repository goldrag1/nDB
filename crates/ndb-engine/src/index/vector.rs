//! Vector index — brute-force CPU k-NN (§14.2 / §17.1).
#![allow(clippy::doc_markdown)] // "HNSW", "IVF", "ScaNN" used liberally.
//!
//! v1 algorithm choice: **brute force**. Iterates every indexed vector
//! per query, computing the configured distance metric. Order of N
//! vectors × D dimensions per query — fine for thousands of vectors,
//! painful past tens of thousands. HNSW / IVF / ScaNN are §14.2's open
//! sub-question and ship as drop-in replacements once a real workload
//! pins the trade-off.
//!
//! Distance metrics (`Distance` enum):
//!
//! - `L2Squared` — squared Euclidean distance. Sortable identically to
//!   Euclidean, skips the `sqrt`. Smaller = closer.
//! - `Cosine` — `1 - cos(θ)`. Vectors are NOT renormalised; the caller
//!   is responsible for storing unit-length vectors if cosine is the
//!   target. Smaller = closer.
//!
//! v1 record-stream integration:
//!
//! - Property IDs flagged via `register_property(prop_id)` are extracted
//!   on every committed entity record.
//! - Only `Value::Vector` payloads contribute.
//! - Tombstones for an entity remove its vector.
//! - Re-assertion replaces the vector (out-of-order replay arrivals
//!   are ignored via the latest-tx watermark, same as `LookupKeyIndex`).
//! - Dimension is captured from the first vector inserted under each
//!   property; subsequent vectors of a different dimension are rejected
//!   silently (logged to stderr in v1).

use std::collections::{HashMap, HashSet};

use crate::id::{EntityId, PropertyId, TxId};
use crate::index::Index;
use crate::record::Record;
use crate::value::Value;

/// Distance metric for vector queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distance {
    /// Squared Euclidean. Smaller = closer.
    L2Squared,
    /// `1 - cos(θ)` for non-normalised vectors. Smaller = closer.
    Cosine,
}

/// Per-property dimension + entries.
#[derive(Debug, Default)]
struct PropertyBucket {
    /// Dimension this property is locked to (from the first vector
    /// inserted). `None` until the first insert.
    dim: Option<usize>,
    /// `entity_id → vector` mapping.
    vectors: HashMap<EntityId, Vec<f32>>,
}

/// In-memory brute-force vector index.
#[derive(Debug, Default)]
pub struct VectorIndex {
    /// Properties declared as vector columns.
    registered: HashSet<PropertyId>,
    /// Per-property storage.
    buckets: HashMap<PropertyId, PropertyBucket>,
    /// Out-of-order detection per entity (one across all property
    /// buckets — entities are re-asserted as a unit).
    latest_tx: HashMap<EntityId, TxId>,
    /// Last-seen list of vector property IDs per entity, so removals
    /// can scrub the correct buckets on tombstone / re-assert.
    entity_props: HashMap<EntityId, Vec<PropertyId>>,
}

impl VectorIndex {
    /// Empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rough estimate of resident heap bytes (diagnostic; walks the
    /// buckets). The embeddings dominate: D × 4 bytes per vector.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        const OVH: usize = 32;
        let mut n = self.registered.len() * (4 + OVH);
        for bucket in self.buckets.values() {
            for v in bucket.vectors.values() {
                n += 16 + 24 + OVH + v.len() * 4; // EntityId + Vec<f32>
            }
        }
        for props in self.entity_props.values() {
            n += 16 + 24 + OVH + props.len() * 4;
        }
        n += self.latest_tx.len() * (16 + 8 + OVH);
        n
    }

    /// Declare `property_id` as carrying vector embeddings. Subsequent
    /// commits will index it. Already-committed entities are NOT
    /// retroactively indexed; call `Engine::rebuild_indexes` to backfill.
    pub fn register_property(&mut self, property_id: PropertyId) {
        self.registered.insert(property_id);
        self.buckets.entry(property_id).or_default();
    }

    /// Whether a property is registered for vector indexing.
    #[must_use]
    pub fn is_registered(&self, property_id: PropertyId) -> bool {
        self.registered.contains(&property_id)
    }

    /// True iff any vector property is registered. Lets flush/compaction
    /// skip building a `.vidx` sidecar when nothing is indexed.
    #[must_use]
    pub fn has_registrations(&self) -> bool {
        !self.registered.is_empty()
    }

    /// Dimension locked into `property_id` (from the first insert), if any.
    #[must_use]
    pub fn dimension(&self, property_id: PropertyId) -> Option<usize> {
        self.buckets.get(&property_id).and_then(|b| b.dim)
    }

    /// How many entities have a vector under `property_id`.
    #[must_use]
    pub fn len(&self, property_id: PropertyId) -> usize {
        self.buckets
            .get(&property_id)
            .map_or(0, |b| b.vectors.len())
    }

    /// Whether the bucket is empty.
    #[must_use]
    pub fn is_empty(&self, property_id: PropertyId) -> bool {
        self.buckets
            .get(&property_id)
            .is_none_or(|b| b.vectors.is_empty())
    }

    /// k-NN search over `property_id`'s bucket. Returns up to `k`
    /// entries sorted ascending by distance. Empty if the bucket is
    /// empty or the query dimension doesn't match.
    #[must_use]
    pub fn search(
        &self,
        property_id: PropertyId,
        query: &[f32],
        k: usize,
        metric: Distance,
    ) -> Vec<(EntityId, f32)> {
        let Some(bucket) = self.buckets.get(&property_id) else {
            return Vec::new();
        };
        if bucket.dim != Some(query.len()) {
            return Vec::new();
        }
        let mut scored: Vec<(EntityId, f32)> = bucket
            .vectors
            .iter()
            .map(|(id, v)| (*id, distance(query, v, metric)))
            .collect();
        // Partial sort by distance ascending; for v1 we sort the whole
        // thing — fine on the brute-force baseline.
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }

    fn apply_entity(&mut self, e: &crate::record::EntityRecord) {
        let eid = e.entity_id;
        if let Some(prev) = self.latest_tx.get(&eid)
            && *prev > e.tx_id_assert
        {
            return;
        }
        // Remove old entries for this entity across every bucket it
        // previously had a vector in.
        self.remove_entity(eid);
        let mut owned: Vec<PropertyId> = Vec::new();
        for (prop, value) in &e.properties {
            if !self.registered.contains(prop) {
                continue;
            }
            let Value::Vector(v) = value else { continue };
            let bucket = self.buckets.entry(*prop).or_default();
            match bucket.dim {
                None => bucket.dim = Some(v.len()),
                Some(d) if d != v.len() => {
                    eprintln!(
                        "vector index: property {} expects dim {d}, got {} — dropping",
                        prop.get(),
                        v.len()
                    );
                    continue;
                }
                _ => {}
            }
            bucket.vectors.insert(eid, v.clone());
            owned.push(*prop);
        }
        if owned.is_empty() {
            self.entity_props.remove(&eid);
        } else {
            self.entity_props.insert(eid, owned);
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
        self.remove_entity(eid);
        self.latest_tx.insert(eid, t.tx_id_supersede);
    }

    fn remove_entity(&mut self, eid: EntityId) {
        if let Some(props) = self.entity_props.remove(&eid) {
            for prop in props {
                if let Some(bucket) = self.buckets.get_mut(&prop) {
                    bucket.vectors.remove(&eid);
                }
            }
        }
    }
}

impl Index for VectorIndex {
    fn apply(&mut self, record: &Record, _tx_id: TxId) {
        match record {
            Record::Entity(e) => self.apply_entity(e),
            Record::Tombstone(t) => self.apply_tombstone(t),
            _ => {}
        }
    }

    fn clear(&mut self) {
        // Preserve registered properties (they're metadata).
        for bucket in self.buckets.values_mut() {
            bucket.dim = None;
            bucket.vectors.clear();
        }
        self.latest_tx.clear();
        self.entity_props.clear();
    }

    fn name(&self) -> &'static str {
        "vector"
    }
}

/// Distance between two equal-length vectors under `metric`. Shared by the
/// in-RAM index and the on-disk `.vidx` reader so both rank identically.
pub(crate) fn distance(a: &[f32], b: &[f32], metric: Distance) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "dimension check happens upstream");
    match metric {
        Distance::L2Squared => a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| {
                let d = x - y;
                d * d
            })
            .sum(),
        Distance::Cosine => {
            let mut dot = 0.0f32;
            let mut na = 0.0f32;
            let mut nb = 0.0f32;
            for (x, y) in a.iter().zip(b.iter()) {
                dot += x * y;
                na += x * x;
                nb += y * y;
            }
            let denom = (na.sqrt() * nb.sqrt()).max(f32::EPSILON);
            1.0 - dot / denom
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::TypeId;
    use crate::record::EntityRecord;

    fn entity(eid: EntityId, tx: u64, prop: u32, v: Vec<f32>) -> Record {
        Record::Entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(tx),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(prop), Value::Vector(v))],
        })
    }

    #[test]
    fn register_and_search_l2() {
        let mut idx = VectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        let a = EntityId::now_v7();
        let b = EntityId::now_v7();
        let c = EntityId::now_v7();
        idx.apply(&entity(a, 1, 10, vec![1.0, 0.0, 0.0]), TxId::new(1));
        idx.apply(&entity(b, 2, 10, vec![0.0, 1.0, 0.0]), TxId::new(2));
        idx.apply(&entity(c, 3, 10, vec![0.9, 0.1, 0.0]), TxId::new(3));
        assert_eq!(idx.len(prop), 3);
        assert_eq!(idx.dimension(prop), Some(3));

        let hits = idx.search(prop, &[1.0, 0.0, 0.0], 2, Distance::L2Squared);
        // a is exact match (distance 0), c is closer to a than b.
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, a);
        assert!(hits[0].1 == 0.0);
        assert_eq!(hits[1].0, c);
    }

    #[test]
    fn cosine_distance_orders_by_direction() {
        let mut idx = VectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        // Two unit-vector-ish points along +x, then a long +x vector
        // (same direction, larger magnitude → cosine = 0, L2 large).
        let near = EntityId::now_v7();
        let far_same_dir = EntityId::now_v7();
        idx.apply(&entity(near, 1, 10, vec![1.0, 0.0]), TxId::new(1));
        idx.apply(&entity(far_same_dir, 2, 10, vec![100.0, 0.0]), TxId::new(2));
        let hits = idx.search(prop, &[1.0, 0.0], 2, Distance::Cosine);
        // Both have cosine distance == 0 (parallel vectors); ordering
        // tied; just check both present.
        assert_eq!(hits.len(), 2);
        for (_, d) in &hits {
            assert!(
                d.abs() < 1e-5,
                "cosine distance for parallel vec should be ~0, got {d}"
            );
        }
    }

    #[test]
    fn tombstone_removes_vector() {
        let mut idx = VectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        let a = EntityId::now_v7();
        idx.apply(&entity(a, 1, 10, vec![1.0, 0.0]), TxId::new(1));
        assert_eq!(idx.len(prop), 1);
        idx.apply(
            &Record::Tombstone(crate::record::TombstoneRecord {
                target_id: a.into_uuid(),
                tx_id_supersede: TxId::new(2),
            }),
            TxId::new(2),
        );
        assert_eq!(idx.len(prop), 0);
    }

    #[test]
    fn reassertion_replaces_vector() {
        let mut idx = VectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        let a = EntityId::now_v7();
        idx.apply(&entity(a, 1, 10, vec![1.0, 0.0]), TxId::new(1));
        idx.apply(&entity(a, 2, 10, vec![0.0, 1.0]), TxId::new(2));
        // Only one entry; the new vector wins.
        assert_eq!(idx.len(prop), 1);
        let hits = idx.search(prop, &[0.0, 1.0], 1, Distance::L2Squared);
        assert_eq!(hits[0].0, a);
        assert!(hits[0].1 == 0.0);
    }

    #[test]
    fn out_of_order_older_assertion_ignored() {
        let mut idx = VectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        let a = EntityId::now_v7();
        // Newer first.
        idx.apply(&entity(a, 5, 10, vec![1.0, 0.0]), TxId::new(5));
        // Older arrives later — must be ignored.
        idx.apply(&entity(a, 1, 10, vec![0.0, 1.0]), TxId::new(1));
        let hits = idx.search(prop, &[1.0, 0.0], 1, Distance::L2Squared);
        assert_eq!(hits[0].0, a);
        assert!(
            hits[0].1 == 0.0,
            "newer vector must win, got distance {}",
            hits[0].1
        );
    }

    #[test]
    fn unregistered_property_ignored() {
        let mut idx = VectorIndex::new();
        // Don't register prop 10.
        let a = EntityId::now_v7();
        idx.apply(&entity(a, 1, 10, vec![1.0, 0.0]), TxId::new(1));
        assert_eq!(idx.len(PropertyId::new(10)), 0);
    }

    #[test]
    fn mismatched_dimension_is_dropped() {
        let mut idx = VectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        idx.apply(
            &entity(EntityId::now_v7(), 1, 10, vec![1.0, 2.0, 3.0]),
            TxId::new(1),
        );
        // 2D vector — different dim, should be silently dropped.
        idx.apply(
            &entity(EntityId::now_v7(), 2, 10, vec![1.0, 2.0]),
            TxId::new(2),
        );
        assert_eq!(idx.len(prop), 1);
        assert_eq!(idx.dimension(prop), Some(3));
    }

    #[test]
    fn k_larger_than_population_returns_all() {
        let mut idx = VectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        idx.apply(&entity(EntityId::now_v7(), 1, 10, vec![1.0]), TxId::new(1));
        idx.apply(&entity(EntityId::now_v7(), 2, 10, vec![2.0]), TxId::new(2));
        let hits = idx.search(prop, &[0.0], 99, Distance::L2Squared);
        assert_eq!(hits.len(), 2);
    }
}
