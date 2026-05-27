//! HNSW (Hierarchical Navigable Small World) vector index for nDB
//! (§14.2 / §17.1).
//!
//! Implements the same external surface as the brute-force baseline
//! (`ndb_engine::VectorIndex`): register a property as vector-bearing,
//! consume committed entity records, answer k-NN search with L2-squared
//! or cosine distance. Internally backed by the `instant-distance` crate
//! (pure-safe-Rust HNSW). Drop-in replacement for the brute-force index
//! once query volume outgrows the linear scan.
//!
//! # Why HNSW for v1 ANN
//!
//! Spec §14.2 listed three candidates: HNSW, IVF, and ScaNN. v1 picks
//! **HNSW** for three reasons:
//!
//! 1. **Maturity.** HNSW has shipped in production at every major vector
//!    DB (Weaviate, Qdrant, Milvus, pgvector, Lucene, Elasticsearch). It
//!    is the best-tested ANN algorithm in the OSS world.
//! 2. **No training step.** IVF needs a centroid-fitting pass over a
//!    sample of the data set; ScaNN needs both centroid fitting and a
//!    learned reordering model. HNSW builds incrementally per point. nDB
//!    workloads cover a wide range of dataset sizes (10⁴ to 10⁸); HNSW
//!    handles both ends without re-configuration.
//! 3. **Pure-Rust dep.** `instant-distance` has zero unsafe code, no C++
//!    toolchain, no SIMD intrinsics. Cross-platform, builds on the same
//!    toolchain as the rest of nDB.
//!
//! IVF and ScaNN remain candidate adds for v2 once production usage
//! pinpoints workloads where their trade-offs win.
//!
//! # v1 design baked in here
//!
//! - **HNSW is rebuilt lazily.** Each `apply()` updates an in-memory
//!   pending map per property and marks the bucket dirty. `search()`
//!   rebuilds the HNSW only when needed (first call, or after dirty
//!   flag). Trade-off: O(N log N) cost on the first search after a
//!   batch of inserts. The recommended pattern is bulk-load → search,
//!   not interleave-write-and-search.
//!
//! - **The brute-force baseline stays as the correctness oracle.** The
//!   crate's own tests check that HNSW agrees with a brute-force search
//!   on the top-1 hit. HNSW is approximate (default `ef_search=100`,
//!   `ef_construction=100`); rare ties may shift but the nearest
//!   neighbor is consistent within the index's accuracy floor.
//!
//! - **Distance metrics inherit from the brute-force surface.**
//!   `Distance::L2Squared` and `Distance::Cosine` are passed through;
//!   the per-property bucket caches the metric of the first search
//!   request so subsequent rebuilds use the same shape (HNSW graph
//!   topology is metric-dependent; mixing metrics on the same bucket
//!   silently rebuilds the graph for each).
//!
//! - **Builder parameters.** The default `Builder` (M=24, ef_construction
//!   =100, ef_search=100) matches `instant-distance`'s recommended
//!   defaults. Future configuration knobs flow through
//!   `register_property_with_config`.

#![warn(missing_docs)]
#![allow(clippy::doc_markdown, clippy::cast_precision_loss)]

use std::collections::{HashMap, HashSet};

use instant_distance::{Builder, HnswMap, Point, Search};
use ndb_engine::id::{EntityId, PropertyId, TxId};
use ndb_engine::index::Index;
use ndb_engine::index::vector::Distance;
use ndb_engine::record::{EntityRecord, Record, TombstoneRecord};
use ndb_engine::value::Value;

/// Wrapped vector + bound metric — implements `instant_distance::Point`.
///
/// The metric is captured at HNSW build time and frozen for the graph's
/// lifetime; this matches the way every production HNSW deployment
/// works (graph topology is metric-specific). To switch metrics on the
/// same bucket, the index rebuilds.
#[derive(Debug, Clone)]
struct MetricPoint {
    vec: Vec<f32>,
    metric: Distance,
}

impl Point for MetricPoint {
    fn distance(&self, other: &Self) -> f32 {
        debug_assert_eq!(
            self.vec.len(),
            other.vec.len(),
            "dimension check happens upstream"
        );
        debug_assert_eq!(
            self.metric, other.metric,
            "metric mismatch should not happen — graph is metric-locked"
        );
        match self.metric {
            Distance::L2Squared => self
                .vec
                .iter()
                .zip(other.vec.iter())
                .map(|(x, y)| {
                    let d = x - y;
                    d * d
                })
                .sum(),
            Distance::Cosine => {
                let mut dot = 0.0f32;
                let mut na = 0.0f32;
                let mut nb = 0.0f32;
                for (x, y) in self.vec.iter().zip(other.vec.iter()) {
                    dot += x * y;
                    na += x * x;
                    nb += y * y;
                }
                let denom = (na.sqrt() * nb.sqrt()).max(f32::EPSILON);
                1.0 - dot / denom
            }
        }
    }
}

/// Per-property bucket: pending state + lazily-built HNSW.
struct PropertyBucket {
    /// Dimension this property is locked to (from the first inserted
    /// vector). `None` until the first insert.
    dim: Option<usize>,
    /// Distance metric the current HNSW (if any) was built with.
    built_metric: Option<Distance>,
    /// In-memory pending state: entity → vector. The source of truth.
    pending: HashMap<EntityId, Vec<f32>>,
    /// Built HNSW + parallel ordered vector of entity ids matching the
    /// HnswMap value indices. `None` until first build.
    built: Option<HnswMap<MetricPoint, EntityId>>,
    /// Dirty since last build.
    dirty: bool,
}

impl Default for PropertyBucket {
    fn default() -> Self {
        Self {
            dim: None,
            built_metric: None,
            pending: HashMap::new(),
            built: None,
            dirty: true,
        }
    }
}

/// HNSW-backed vector index — mirrors `ndb_engine::VectorIndex`'s surface.
pub struct HnswVectorIndex {
    registered: HashSet<PropertyId>,
    buckets: HashMap<PropertyId, PropertyBucket>,
    latest_tx: HashMap<EntityId, TxId>,
    entity_props: HashMap<EntityId, Vec<PropertyId>>,
    /// Builder template — every fresh rebuild starts from these settings.
    builder_template: BuilderConfig,
}

/// User-tunable HNSW builder knobs. Default matches `instant-distance`
/// out-of-the-box.
#[derive(Debug, Clone, Copy)]
pub struct BuilderConfig {
    /// `ef_construction` — neighbour-candidate pool during build. Higher
    /// = better accuracy, slower build.
    pub ef_construction: usize,
    /// `ef_search` — neighbour-candidate pool during search. Higher =
    /// better recall, slower per-query.
    pub ef_search: usize,
    /// Deterministic RNG seed for reproducible graphs.
    pub seed: u64,
}

impl Default for BuilderConfig {
    fn default() -> Self {
        Self {
            ef_construction: 100,
            ef_search: 100,
            seed: 0,
        }
    }
}

impl std::fmt::Debug for HnswVectorIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // HnswMap doesn't implement Debug, so we summarise the bucket
        // count instead of listing each.
        f.debug_struct("HnswVectorIndex")
            .field("registered", &self.registered)
            .field("bucket_count", &self.buckets.len())
            .field("config", &self.builder_template)
            .field("entity_count", &self.entity_props.len())
            .field("latest_tx_count", &self.latest_tx.len())
            .finish()
    }
}

impl Default for HnswVectorIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl HnswVectorIndex {
    /// Empty index with default builder parameters.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(BuilderConfig::default())
    }

    /// Empty index with caller-supplied builder parameters. Affects every
    /// subsequent HNSW build (existing built indexes are not retrofitted
    /// until the next dirty-rebuild).
    #[must_use]
    pub fn with_config(config: BuilderConfig) -> Self {
        Self {
            registered: HashSet::new(),
            buckets: HashMap::new(),
            latest_tx: HashMap::new(),
            entity_props: HashMap::new(),
            builder_template: config,
        }
    }

    /// Mark `property_id` as vector-bearing. Same semantics as the
    /// brute-force index.
    pub fn register_property(&mut self, property_id: PropertyId) {
        self.registered.insert(property_id);
        self.buckets.entry(property_id).or_default();
    }

    /// Whether a property is registered.
    #[must_use]
    pub fn is_registered(&self, property_id: PropertyId) -> bool {
        self.registered.contains(&property_id)
    }

    /// Dimension locked into `property_id`, if any.
    #[must_use]
    pub fn dimension(&self, property_id: PropertyId) -> Option<usize> {
        self.buckets.get(&property_id).and_then(|b| b.dim)
    }

    /// Number of indexed vectors under `property_id`.
    #[must_use]
    pub fn len(&self, property_id: PropertyId) -> usize {
        self.buckets
            .get(&property_id)
            .map_or(0, |b| b.pending.len())
    }

    /// Whether the bucket is empty.
    #[must_use]
    pub fn is_empty(&self, property_id: PropertyId) -> bool {
        self.buckets
            .get(&property_id)
            .is_none_or(|b| b.pending.is_empty())
    }

    /// k-NN search. Triggers a graph rebuild on first call or after
    /// dirty inserts/removals. Returns up to `k` entries sorted ascending
    /// by approximate distance.
    pub fn search(
        &mut self,
        property_id: PropertyId,
        query: &[f32],
        k: usize,
        metric: Distance,
    ) -> Vec<(EntityId, f32)> {
        let template = self.builder_template;
        let Some(bucket) = self.buckets.get_mut(&property_id) else {
            return Vec::new();
        };
        if bucket.dim != Some(query.len()) {
            return Vec::new();
        }
        if bucket.pending.is_empty() {
            return Vec::new();
        }
        // Rebuild when dirty or when the metric differs from the
        // previously-built one.
        let need_rebuild = bucket.dirty || bucket.built_metric != Some(metric);
        if need_rebuild {
            rebuild_bucket(bucket, metric, template);
        }
        let built = bucket
            .built
            .as_ref()
            .expect("rebuild left built map populated");
        let query_point = MetricPoint {
            vec: query.to_vec(),
            metric,
        };
        let mut search = Search::default();
        let mut out: Vec<(EntityId, f32)> = built
            .search(&query_point, &mut search)
            .take(k)
            .map(|item| (*item.value, item.distance))
            .collect();
        out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        out
    }

    fn apply_entity(&mut self, e: &EntityRecord) {
        let eid = e.entity_id;
        if let Some(prev) = self.latest_tx.get(&eid)
            && *prev > e.tx_id_assert
        {
            return;
        }
        self.remove_entity_inner(eid);
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
                        "hnsw vector index: property {} expects dim {d}, got {} — dropping",
                        prop.get(),
                        v.len(),
                    );
                    continue;
                }
                _ => {}
            }
            bucket.pending.insert(eid, v.clone());
            bucket.dirty = true;
            owned.push(*prop);
        }
        if owned.is_empty() {
            self.entity_props.remove(&eid);
        } else {
            self.entity_props.insert(eid, owned);
        }
        self.latest_tx.insert(eid, e.tx_id_assert);
    }

    fn apply_tombstone(&mut self, t: &TombstoneRecord) {
        let eid = EntityId::from_uuid(t.target_id);
        if let Some(prev) = self.latest_tx.get(&eid)
            && *prev > t.tx_id_supersede
        {
            return;
        }
        self.remove_entity_inner(eid);
        self.latest_tx.insert(eid, t.tx_id_supersede);
    }

    fn remove_entity_inner(&mut self, eid: EntityId) {
        if let Some(props) = self.entity_props.remove(&eid) {
            for prop in props {
                if let Some(bucket) = self.buckets.get_mut(&prop) {
                    bucket.pending.remove(&eid);
                    bucket.dirty = true;
                }
            }
        }
    }
}

fn rebuild_bucket(bucket: &mut PropertyBucket, metric: Distance, config: BuilderConfig) {
    if bucket.pending.is_empty() {
        bucket.built = None;
        bucket.built_metric = None;
        bucket.dirty = false;
        return;
    }
    let mut points: Vec<MetricPoint> = Vec::with_capacity(bucket.pending.len());
    let mut values: Vec<EntityId> = Vec::with_capacity(bucket.pending.len());
    for (eid, v) in &bucket.pending {
        points.push(MetricPoint {
            vec: v.clone(),
            metric,
        });
        values.push(*eid);
    }
    let map = Builder::default()
        .ef_construction(config.ef_construction)
        .ef_search(config.ef_search)
        .seed(config.seed)
        .build(points, values);
    bucket.built = Some(map);
    bucket.built_metric = Some(metric);
    bucket.dirty = false;
}

impl Index for HnswVectorIndex {
    fn apply(&mut self, record: &Record, _tx_id: TxId) {
        match record {
            Record::Entity(e) => self.apply_entity(e),
            Record::Tombstone(t) => self.apply_tombstone(t),
            _ => {}
        }
    }

    fn clear(&mut self) {
        for bucket in self.buckets.values_mut() {
            bucket.dim = None;
            bucket.built_metric = None;
            bucket.pending.clear();
            bucket.built = None;
            bucket.dirty = true;
        }
        self.latest_tx.clear();
        self.entity_props.clear();
    }

    fn name(&self) -> &'static str {
        "vector-hnsw"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndb_engine::id::TypeId;
    use ndb_engine::record::EntityRecord;
    use ndb_engine::value::Value;

    fn entity(eid: EntityId, tx: u64, prop: u32, v: Vec<f32>) -> Record {
        Record::Entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(tx),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(prop), Value::Vector(v))],
        })
    }

    fn id(b: u8) -> EntityId {
        EntityId::from_bytes([
            b, b, b, b, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, b,
        ])
    }

    #[test]
    fn register_and_search_finds_exact_match_first() {
        let mut idx = HnswVectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        let a = id(1);
        let b = id(2);
        let c = id(3);
        idx.apply(&entity(a, 1, 10, vec![1.0, 0.0, 0.0]), TxId::new(1));
        idx.apply(&entity(b, 2, 10, vec![0.0, 1.0, 0.0]), TxId::new(2));
        idx.apply(&entity(c, 3, 10, vec![0.9, 0.1, 0.0]), TxId::new(3));
        assert_eq!(idx.len(prop), 3);
        assert_eq!(idx.dimension(prop), Some(3));

        let hits = idx.search(prop, &[1.0, 0.0, 0.0], 2, Distance::L2Squared);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, a);
        assert!((hits[0].1).abs() < 1e-6);
        assert_eq!(hits[1].0, c);
    }

    #[test]
    fn unregistered_property_returns_empty() {
        let mut idx = HnswVectorIndex::new();
        let hits = idx.search(PropertyId::new(99), &[1.0, 0.0], 5, Distance::L2Squared);
        assert!(hits.is_empty());
    }

    #[test]
    fn dimension_mismatch_drops_silently() {
        let mut idx = HnswVectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        idx.apply(&entity(id(1), 1, 10, vec![1.0, 0.0, 0.0]), TxId::new(1));
        // Different dimension — should be dropped.
        idx.apply(&entity(id(2), 2, 10, vec![1.0, 0.0]), TxId::new(2));
        assert_eq!(idx.len(prop), 1);
    }

    #[test]
    fn query_dimension_mismatch_returns_empty() {
        let mut idx = HnswVectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        idx.apply(&entity(id(1), 1, 10, vec![1.0, 0.0, 0.0]), TxId::new(1));
        let hits = idx.search(prop, &[1.0, 0.0], 2, Distance::L2Squared);
        assert!(hits.is_empty());
    }

    #[test]
    fn tombstone_removes_entity_from_results() {
        let mut idx = HnswVectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        let a = id(1);
        let b = id(2);
        idx.apply(&entity(a, 1, 10, vec![1.0, 0.0, 0.0]), TxId::new(1));
        idx.apply(&entity(b, 2, 10, vec![0.0, 1.0, 0.0]), TxId::new(2));
        idx.apply(
            &Record::Tombstone(TombstoneRecord {
                target_id: a.into_uuid(),
                tx_id_supersede: TxId::new(3),
            }),
            TxId::new(3),
        );
        assert_eq!(idx.len(prop), 1);
        let hits = idx.search(prop, &[1.0, 0.0, 0.0], 5, Distance::L2Squared);
        // Tombstoned vector should not appear.
        assert!(hits.iter().all(|(eid, _)| *eid != a));
    }

    #[test]
    fn out_of_order_replay_ignored() {
        let mut idx = HnswVectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        let a = id(1);
        // tx_id 5 first, then a stale tx_id 3 — the stale one should
        // be dropped by the latest-tx watermark.
        idx.apply(&entity(a, 5, 10, vec![1.0, 0.0]), TxId::new(5));
        idx.apply(&entity(a, 3, 10, vec![5.0, 5.0]), TxId::new(3));
        let hits = idx.search(prop, &[1.0, 0.0], 1, Distance::L2Squared);
        assert_eq!(hits.len(), 1);
        assert!((hits[0].1).abs() < 1e-6, "first vector should still be the live one");
    }

    #[test]
    fn re_assertion_replaces_vector() {
        let mut idx = HnswVectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        let a = id(1);
        idx.apply(&entity(a, 1, 10, vec![1.0, 0.0]), TxId::new(1));
        idx.apply(&entity(a, 2, 10, vec![0.0, 1.0]), TxId::new(2));
        let hits = idx.search(prop, &[0.0, 1.0], 1, Distance::L2Squared);
        assert_eq!(hits[0].0, a);
        assert!((hits[0].1).abs() < 1e-6);
    }

    #[test]
    fn clear_resets_state_but_keeps_registration() {
        let mut idx = HnswVectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        idx.apply(&entity(id(1), 1, 10, vec![1.0, 0.0]), TxId::new(1));
        assert_eq!(idx.len(prop), 1);
        idx.clear();
        assert_eq!(idx.len(prop), 0);
        assert!(idx.is_registered(prop));
    }

    #[test]
    fn matches_brute_force_top1_on_random_dataset() {
        // 200 random 8-dim vectors. HNSW should return the same top-1
        // hit as a brute-force scan.
        use ndb_engine::index::vector::VectorIndex;

        let mut hnsw = HnswVectorIndex::new();
        let mut bf = VectorIndex::new();
        let prop = PropertyId::new(7);
        hnsw.register_property(prop);
        bf.register_property(prop);

        // Deterministic pseudo-random fill.
        let mut state: u32 = 0x1234_5678;
        let mut next = || -> f32 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (state as f32) / (u32::MAX as f32)
        };

        let mut ids: Vec<EntityId> = Vec::new();
        for i in 0..200u8 {
            let v: Vec<f32> = (0..8).map(|_| next()).collect();
            // Construct an EntityId that's unique per i (the id() helper
            // collides at i==0).
            let synthetic = EntityId::from_bytes([
                i.wrapping_add(1),
                i,
                i,
                i,
                0x70,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                i,
            ]);
            ids.push(synthetic);
            hnsw.apply(
                &entity(synthetic, u64::from(i) + 1, 7, v.clone()),
                TxId::new(u64::from(i) + 1),
            );
            bf.apply(
                &entity(synthetic, u64::from(i) + 1, 7, v),
                TxId::new(u64::from(i) + 1),
            );
        }

        let query: Vec<f32> = (0..8).map(|_| next()).collect();
        let bf_hits = bf.search(prop, &query, 1, Distance::L2Squared);
        let hnsw_hits = hnsw.search(prop, &query, 1, Distance::L2Squared);
        assert_eq!(bf_hits.len(), 1);
        assert_eq!(hnsw_hits.len(), 1);
        assert_eq!(
            bf_hits[0].0, hnsw_hits[0].0,
            "HNSW top-1 should match brute-force top-1 on this dataset",
        );
    }

    #[test]
    fn cosine_metric_works() {
        let mut idx = HnswVectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        // Three vectors. The one pointing in the same direction as query
        // should be returned first regardless of magnitude.
        idx.apply(&entity(id(1), 1, 10, vec![1.0, 0.0]), TxId::new(1));
        idx.apply(&entity(id(2), 2, 10, vec![100.0, 0.0]), TxId::new(2));
        idx.apply(&entity(id(3), 3, 10, vec![0.0, 1.0]), TxId::new(3));
        let hits = idx.search(prop, &[5.0, 0.0], 2, Distance::Cosine);
        assert_eq!(hits.len(), 2);
        // Top 2 should be id(1) and id(2) (both parallel to query).
        for (eid, _) in &hits {
            assert!(*eid != id(3), "cosine should rank perpendicular last");
        }
    }

    #[test]
    fn k_capped_to_available() {
        let mut idx = HnswVectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        idx.apply(&entity(id(1), 1, 10, vec![1.0, 0.0]), TxId::new(1));
        idx.apply(&entity(id(2), 2, 10, vec![0.0, 1.0]), TxId::new(2));
        let hits = idx.search(prop, &[1.0, 0.0], 10, Distance::L2Squared);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn empty_bucket_returns_empty() {
        let mut idx = HnswVectorIndex::new();
        let prop = PropertyId::new(10);
        idx.register_property(prop);
        let hits = idx.search(prop, &[1.0, 0.0], 5, Distance::L2Squared);
        assert!(hits.is_empty());
    }

    #[test]
    fn name_is_distinct_from_brute_force() {
        let idx = HnswVectorIndex::new();
        assert_eq!(idx.name(), "vector-hnsw");
    }
}
