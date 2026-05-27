//! Engine — the runtime that ties [`Database`], [`WriteAheadLog`],
//! [`Memtable`], and the open SSTable readers into one usable handle.
#![allow(clippy::doc_markdown)] // "Engine", "Database", "SSTable", "WAL" used liberally.
//!
//! v1 surface (intentionally narrow):
//!
//! - `Engine::create(path)` — make a fresh database directory and a
//!   first WAL.
//! - `Engine::open(path)` — acquire the LOCK, load the MANIFEST, open
//!   every active SSTable for read, attach the active WAL and replay
//!   its records into a fresh memtable.
//! - `Engine::begin_write()` — start a write transaction. Allocates a
//!   `TxId` and buffers records in memory; nothing touches disk until
//!   `commit()`.
//! - `WriteTxn::commit()` — encode all records, append them to the WAL,
//!   `fsync_data` the WAL, then insert into the memtable. Each record
//!   gets its `tx_id_assert` (or `tx_id_supersede` for tombstones) stamped
//!   with the transaction's id.
//! - `Engine::snapshot_read(uuid, snapshot)` — MVCC lookup across the
//!   memtable and every open SSTable, newest layer first. Returns a
//!   `Resolved<Record>` so callers see Missing / Deleted / Live cleanly.
//! - `Engine::flush()` — drain the memtable into a new SSTable, update
//!   the MANIFEST, rotate the WAL, and open a fresh memtable. Old WAL is
//!   safe to delete after `MANIFEST` + `CURRENT` are durable; we leave
//!   the old `.ndblog` on disk for one cycle as a belt-and-braces safety
//!   net.
//! - `Engine::close()` — `fsync` the WAL, release the LOCK.
//!
//! Single-writer model (§14.3). The engine is `&mut self` for writes and
//! `&self` for reads, so the caller serialises writers itself; the data
//! structures do not embed locks.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::db::{Database, DatabaseError, Manifest, ManifestEntry};
use crate::error::EncodeError;
use crate::id::{EntityId, HyperedgeId, PropertyId, TX_ACTIVE, TxId, TypeId};
use crate::index::{
    AdjacencyIndex, Distance, HyperEdgeTypeIndex, Index, LookupKeyIndex, PropertyBTreeIndex,
    VectorIndex,
};
use crate::memtable::Memtable;
use crate::mvcc::{Resolved, resolve_iter};
use crate::record::{EntityRecord, HyperEdgeRecord, Record, TombstoneRecord};
use crate::sstable::{SSTableError, SSTableKey, SSTableReader, SSTableWriter};
use crate::validation::{ValidationEngine, ValidationError};
use crate::value::Value;
use crate::wal::{WalReadError, WalReader, WriteAheadLog, truncate_to};

const WAL_FILENAME_SUFFIX: &str = ".ndblog";
const SSTABLE_FILENAME_SUFFIX: &str = ".ndb";

/// Statistics returned by [`Engine::compact`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactionStats {
    /// Records read across all input SSTables.
    pub records_in: u64,
    /// Records written to the new SSTable (after dropping superseded
    /// versions + tombstoned records).
    pub records_out: u64,
    /// Number of input SSTables consumed.
    pub sstables_in: usize,
    /// `file_seq` of the new SSTable. `None` if compaction was a no-op
    /// (zero input SSTables).
    pub new_sstable_seq: Option<u64>,
}

/// Errors raised by the engine layer.
#[derive(Debug, Error)]
pub enum EngineError {
    /// Database-directory error (LOCK, MANIFEST, CURRENT).
    #[error(transparent)]
    Database(#[from] DatabaseError),

    /// WAL read error during recovery.
    #[error(transparent)]
    WalRead(#[from] WalReadError),

    /// SSTable error during write or read.
    #[error(transparent)]
    SSTable(#[from] SSTableError),

    /// Record encode failure (size overflow, sentinel violation).
    #[error(transparent)]
    Encode(#[from] EncodeError),

    /// I/O error not already classified.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Caller tried to commit a record whose `tx_id_assert` does not match
    /// the current transaction's id.
    #[error("record's tx_id does not match this transaction (record={record_tx}, tx={txn_tx})")]
    TxIdMismatch {
        /// Tx id stamped on the offending record.
        record_tx: u64,
        /// Transaction's actual id.
        txn_tx: u64,
    },

    /// Validation engine rejected a record (missing required property,
    /// wrong value tag, etc.).
    #[error(transparent)]
    Validation(#[from] ValidationError),

    /// Serializable transaction aborted at commit because a key in its
    /// read-set was modified by a later-committed transaction.
    ///
    /// v1 single-writer engine cannot produce this error in practice;
    /// the variant exists for the v2 multi-writer / distributed
    /// surface. See [`IsolationLevel::Serializable`].
    #[error(
        "serialization_failure: read key {key:?} modified at tx {modified_at} after snapshot tx {read_at}"
    )]
    SerializationFailure {
        /// UUID of the read-set key whose state changed.
        key: uuid::Uuid,
        /// Snapshot tx_id when the key was read.
        read_at: u64,
        /// Tx_id at which the key was modified after the read.
        modified_at: u64,
    },
}

// ---------------------------------------------------------------------------
// Isolation levels (§10.2)
// ---------------------------------------------------------------------------

/// Per-transaction isolation level. Caller specifies via
/// [`WriteTxn::with_isolation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationLevel {
    /// Snapshot Isolation — default. Each transaction sees its consistent
    /// snapshot. Write-skew anomalies are possible if the application has
    /// invariants that span multiple keys. Highest throughput.
    #[default]
    SnapshotIsolation,
    /// Serializable Snapshot Isolation. SI + conflict detection at commit
    /// time. The engine tracks the read-set (per call to
    /// [`WriteTxn::read`]) and aborts the commit if any of those keys
    /// was modified by a later-committed transaction since the read.
    ///
    /// v1 reality check: the engine is single-writer (`begin_write` takes
    /// `&mut Engine`), so concurrent writes don't exist and the conflict
    /// detection is structurally trivial — it never aborts in a
    /// single-process v1 workload. The API surface lands here so callers
    /// can opt into the stronger guarantee, and the conflict-check code
    /// path is ready for v2 multi-writer / distributed mode without
    /// changing client code.
    Serializable,
}

// ---------------------------------------------------------------------------
// Per-type retention policies (§17.1)
// ---------------------------------------------------------------------------

/// How many versions of a key the compactor should retain for a given
/// type. Applied per `(type_id, key)` group at compaction time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetentionPolicy {
    /// Keep only the snapshot-visible version, drop all superseded
    /// versions and tombstones once they've done their job. Default.
    /// Lowest storage; fastest reads; no version history.
    #[default]
    LatestOnly,
    /// Keep the latest N versions (visible winner + up to N-1 older
    /// superseded versions). `N = 0` is equivalent to `LatestOnly`;
    /// `N = 1` keeps only the live one + one tombstone if present.
    Versioned {
        /// Number of versions to keep (≥ 1; effective minimum 1).
        keep_last_n: u32,
    },
    /// Keep every version forever. Highest storage; full audit trail.
    /// Tombstones are also retained — readers always see the complete
    /// version chain.
    Audited,
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Single-process database engine handle.
#[derive(Debug)]
pub struct Engine {
    /// Memtable comes before db so it's dropped first on panic (no LOCK
    /// release waiting on a dangling memtable).
    memtable: Memtable,
    /// Open readers for every SSTable listed in the active MANIFEST, in
    /// the order they appear in `Manifest::sstables`. New SSTables are
    /// prepended on flush so newest layer is index 0.
    sstables: Vec<SSTableReader>,
    /// Active WAL, kept open for append. `None` only during create()
    /// before the first WAL exists.
    wal: Option<WriteAheadLog>,
    /// Lookup-key reverse index — `(property_id, value) → entity_id`.
    lookup_key: LookupKeyIndex,
    /// Adjacency index — `entity → [hyperedges referencing it]`.
    adjacency: AdjacencyIndex,
    /// Hyperedge-type clustering — `type_id → [hyperedge ids]`.
    type_cluster: HyperEdgeTypeIndex,
    /// Brute-force vector index for k-NN search over embedding props.
    vector: VectorIndex,
    /// Property B-tree — `(type, prop, value) → entities` for exact +
    /// range queries on registered columns.
    property_btree: PropertyBTreeIndex,
    /// Constraint enforcement (required properties, value-tag checks).
    validation: ValidationEngine,
    /// Database directory handle (owns the LOCK + current MANIFEST).
    db: Database,
    /// In-memory map of `tx_id → commit_timestamp_us`. Populated at
    /// commit time. v1 limitation: session-local — not persisted across
    /// engine open/close. `as of "<timestamp>"` queries against tx_ids
    /// committed in this process work; queries against pre-restart
    /// tx_ids return `TimestampUnavailable`. v2 will persist via a new
    /// `TxTimestampRecord` kind or the MANIFEST.
    commit_timestamps: std::collections::BTreeMap<TxId, i64>,
    /// Per-type retention policy. Compactor consults this when deciding
    /// how many superseded versions to retain for each `(type, key)`
    /// group. Types not present default to `LatestOnly`. Same in-memory
    /// caveat as `commit_timestamps` — v1 session-local; v2 persists.
    retention: HashMap<TypeId, RetentionPolicy>,
}

impl Engine {
    /// Create a fresh database directory and engine.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, EngineError> {
        let mut db = Database::create(path)?;
        let wal_seq = db.allocate_file_seq();
        let wal_path = wal_path(db.path(), wal_seq);
        let wal = WriteAheadLog::create(&wal_path)?;
        let mut manifest = db.manifest().clone();
        manifest.active_wal_seq = wal_seq;
        db.write_manifest(manifest)?;
        Ok(Self {
            memtable: Memtable::new(),
            sstables: Vec::new(),
            wal: Some(wal),
            lookup_key: LookupKeyIndex::new(),
            adjacency: AdjacencyIndex::new(),
            type_cluster: HyperEdgeTypeIndex::new(),
            vector: VectorIndex::new(),
            property_btree: PropertyBTreeIndex::new(),
            validation: ValidationEngine::new(),
            db,
            commit_timestamps: std::collections::BTreeMap::new(),
            retention: HashMap::new(),
        })
    }

    /// Open an existing database directory.
    ///
    /// Recovery flow:
    /// 1. Acquire LOCK (via `Database::open`).
    /// 2. Open SSTables listed in MANIFEST (newest level first).
    /// 3. If `active_wal_seq != 0`, scan the WAL: recover() detects torn
    ///    trailing records, truncates to the safe boundary, then replays
    ///    every clean record into a fresh memtable.
    /// 4. If `active_wal_seq == 0`, mint a new WAL and persist its seq.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, EngineError> {
        let mut db = Database::open(path)?;

        // Open active SSTables.
        let mut sstables: Vec<SSTableReader> = Vec::new();
        // Sort entries by level then file_seq descending so newest layer is
        // first in the lookup chain.
        let mut entries = db.manifest().sstables.clone();
        entries.sort_by(|a, b| a.level.cmp(&b.level).then(b.file_seq.cmp(&a.file_seq)));
        for entry in &entries {
            let p = sstable_path(db.path(), entry.file_seq);
            sstables.push(SSTableReader::open(&p)?);
        }

        // Replay WAL (or mint a fresh one).
        let mut memtable = Memtable::new();
        let wal_seq = db.manifest().active_wal_seq;
        let wal = if wal_seq == 0 {
            let new_seq = db.allocate_file_seq();
            let p = wal_path(db.path(), new_seq);
            let mut m = db.manifest().clone();
            m.active_wal_seq = new_seq;
            db.write_manifest(m)?;
            WriteAheadLog::create(&p)?
        } else {
            let p = wal_path(db.path(), wal_seq);
            let (safe_end, max_tx_seen) = replay_wal_into(&p, &mut memtable)?;
            truncate_to(&p, safe_end)?;
            if max_tx_seen > db.manifest().last_tx_id {
                // Reconcile the MANIFEST with what the WAL just told us
                // happened since the last flush. Persist immediately so a
                // subsequent crash before the next flush doesn't re-stale
                // the watermark.
                let mut m = db.manifest().clone();
                m.last_tx_id = max_tx_seen;
                db.write_manifest(m)?;
            }
            WriteAheadLog::open_append(&p)?
        };

        let mut engine = Self {
            memtable,
            sstables,
            wal: Some(wal),
            lookup_key: LookupKeyIndex::new(),
            adjacency: AdjacencyIndex::new(),
            type_cluster: HyperEdgeTypeIndex::new(),
            vector: VectorIndex::new(),
            property_btree: PropertyBTreeIndex::new(),
            validation: ValidationEngine::new(),
            db,
            commit_timestamps: std::collections::BTreeMap::new(),
            retention: HashMap::new(),
        };
        // Indexes are in-memory in v1 — rebuild them from the primary
        // store (SSTables in newest-first order) and the memtable
        // (already populated from WAL replay).
        engine.rebuild_indexes()?;
        // Metadata-driven validation constraints: scan every visible
        // record at the latest tx and register any constraint entities
        // with the validation engine. Durable across restarts because
        // they live in the primary store.
        engine.reload_constraints_from_metadata()?;
        Ok(engine)
    }

    /// Scan the latest snapshot for metadata constraint entities and
    /// register them with the validation engine. Returns the number of
    /// constraints loaded. Called automatically by `open()`; callers
    /// that add constraint entities at runtime can invoke this manually
    /// to pick up the changes.
    pub fn reload_constraints_from_metadata(&mut self) -> Result<usize, EngineError> {
        let snap = TxId::new(self.db.manifest().last_tx_id);
        let records = self.snapshot_iter(snap)?;
        Ok(self.validation.load_from_metadata(&records))
    }

    /// Rebuild every in-memory index from the primary store. Called on
    /// `open()` after SSTables are loaded and the memtable is replayed.
    /// Also useful after `register_lookup_key` if the caller wants the
    /// new property backfilled over already-loaded records.
    ///
    /// Order: SSTables newest-first, then memtable. Records arriving with
    /// older `tx_id_assert` are ignored by each index's out-of-order
    /// guard, so the "newest wins" property holds regardless of replay
    /// ordering.
    pub fn rebuild_indexes(&mut self) -> Result<(), EngineError> {
        self.lookup_key.clear();
        self.adjacency.clear();
        self.type_cluster.clear();
        self.vector.clear();
        self.property_btree.clear();
        // Metadata maps (v2.0+) — rebuilt from the durable records.
        self.commit_timestamps.clear();
        self.retention.clear();
        // SSTables (sstables[0] is newest layer; iterate in declared order).
        for sst in &mut self.sstables {
            for item in sst.iter() {
                let (rec, _) = item?;
                let tx = match &rec {
                    Record::Entity(e) => e.tx_id_assert,
                    Record::HyperEdge(h) => h.tx_id_assert,
                    Record::Tombstone(t) => t.tx_id_supersede,
                    Record::TxTimestamp(t) => t.tx_id,
                    _ => TxId::new(0),
                };
                match &rec {
                    Record::TxTimestamp(t) => {
                        self.commit_timestamps.insert(t.tx_id, t.timestamp_us);
                    }
                    Record::RetentionPolicy(rp) => {
                        if let Some(p) = decode_retention_policy(rp.policy_kind, rp.keep_last_n) {
                            self.retention.insert(rp.type_id, p);
                        }
                    }
                    _ => {}
                }
                self.lookup_key.apply(&rec, tx);
                self.adjacency.apply(&rec, tx);
                self.type_cluster.apply(&rec, tx);
                self.vector.apply(&rec, tx);
                self.property_btree.apply(&rec, tx);
            }
        }
        // Memtable.
        for (_k, rec) in self.memtable.iter() {
            let tx = match rec {
                Record::Entity(e) => e.tx_id_assert,
                Record::HyperEdge(h) => h.tx_id_assert,
                Record::Tombstone(t) => t.tx_id_supersede,
                Record::TxTimestamp(t) => t.tx_id,
                _ => TxId::new(0),
            };
            match rec {
                Record::TxTimestamp(t) => {
                    self.commit_timestamps.insert(t.tx_id, t.timestamp_us);
                }
                Record::RetentionPolicy(rp) => {
                    if let Some(p) = decode_retention_policy(rp.policy_kind, rp.keep_last_n) {
                        self.retention.insert(rp.type_id, p);
                    }
                }
                _ => {}
            }
            self.lookup_key.apply(rec, tx);
            self.adjacency.apply(rec, tx);
            self.type_cluster.apply(rec, tx);
            self.vector.apply(rec, tx);
            self.property_btree.apply(rec, tx);
        }
        Ok(())
    }

    /// Declare a property as REQUIRED on entities of a given type.
    /// Commits that contain an entity of `type_id` missing `property_id`
    /// are rejected with [`ValidationError::MissingRequiredProperty`].
    pub fn require_property(&mut self, type_id: TypeId, property_id: PropertyId) {
        self.validation.require_property(type_id, property_id);
    }

    /// Declare that a property must use a specific `Value` tag byte.
    /// Use the `TAG_*` constants from the `value` module.
    pub fn expect_value_tag(&mut self, type_id: TypeId, property_id: PropertyId, tag: u8) {
        self.validation.expect_value_tag(type_id, property_id, tag);
    }

    /// Borrow the validation engine immutably (for diagnostics).
    #[must_use]
    pub fn validation(&self) -> &ValidationEngine {
        &self.validation
    }

    /// Register a property id as a lookup key. Subsequent commits will
    /// populate the lookup-key index for that property. Already-committed
    /// records will NOT be retroactively indexed — call `rebuild_indexes`
    /// after registration if you need backfill.
    pub fn register_lookup_key(&mut self, property_id: PropertyId) {
        self.lookup_key.register_property(property_id);
    }

    /// Find an entity by an external lookup-key value.
    #[must_use]
    pub fn lookup_by_external_key(
        &self,
        property_id: PropertyId,
        value: &Value,
    ) -> Option<EntityId> {
        self.lookup_key.lookup(property_id, value)
    }

    /// All hyperedges that reference `entity` in any role.
    #[must_use]
    pub fn hyperedges_for_entity(&self, entity: EntityId) -> Vec<HyperedgeId> {
        self.adjacency.neighbors_vec(entity)
    }

    /// All hyperedges of the given type.
    #[must_use]
    pub fn hyperedges_by_type(&self, type_id: TypeId) -> Vec<HyperedgeId> {
        self.type_cluster.by_type_vec(type_id)
    }

    /// Count of hyperedges of `type_id`. Constant-time index probe; used
    /// by the planner to estimate cardinality without materialising.
    #[must_use]
    pub fn hyperedge_type_count(&self, type_id: TypeId) -> usize {
        self.type_cluster.count(type_id)
    }

    /// Degree of `entity` in the adjacency index — number of hyperedges
    /// that name it in any role. Planner uses this for hyperedge atoms
    /// with at least one role bound to a concrete entity.
    #[must_use]
    pub fn adjacency_degree(&self, entity: EntityId) -> usize {
        self.adjacency.degree(entity)
    }

    /// Total hyperedges tracked by the adjacency index, and the count of
    /// distinct entities that participate in at least one. Used by the
    /// planner to compute an average-degree estimate when no role is
    /// bound yet.
    #[must_use]
    pub fn adjacency_overview(&self) -> (usize, usize) {
        (
            self.adjacency.hyperedge_count(),
            self.adjacency.entity_count(),
        )
    }

    /// Whether `(type_id, property_id)` has a property B-tree index.
    /// Planner uses this to decide whether a literal-eq filter can give
    /// an exact cardinality estimate.
    #[must_use]
    pub fn property_btree_registered(&self, type_id: TypeId, property_id: PropertyId) -> bool {
        self.property_btree.is_registered(type_id, property_id)
    }

    /// Declare an entity property as carrying vector embeddings. Subsequent
    /// commits will index it for k-NN search. Already-committed entities
    /// are NOT retroactively indexed — call `rebuild_indexes` after late
    /// registration if you need backfill.
    pub fn register_vector_property(&mut self, property_id: PropertyId) {
        self.vector.register_property(property_id);
    }

    /// Declare a `(type_id, property_id)` pair for B-tree indexing. Enables
    /// `property_lookup` (exact) and `property_range` (sorted range) queries
    /// scoped to that type/property combination.
    pub fn register_property_btree(&mut self, type_id: TypeId, property_id: PropertyId) {
        self.property_btree.register(type_id, property_id);
    }

    /// Exact-match lookup: every entity of `type_id` whose `property_id`
    /// equals `value`. Empty if the pair isn't registered or no match.
    #[must_use]
    pub fn property_lookup(
        &self,
        type_id: TypeId,
        property_id: PropertyId,
        value: &Value,
    ) -> Vec<EntityId> {
        self.property_btree.find(type_id, property_id, value)
    }

    /// Range lookup: every entity of `type_id` whose `property_id` value
    /// falls in `[low, high]` (inclusive on both sides; `None` =
    /// unbounded). Useful for "all customers with age in 18..=65" style
    /// queries.
    #[must_use]
    pub fn property_range(
        &self,
        type_id: TypeId,
        property_id: PropertyId,
        low: Option<&Value>,
        high: Option<&Value>,
    ) -> Vec<EntityId> {
        self.property_btree.range(type_id, property_id, low, high)
    }

    /// k-nearest-neighbor search over a vector-indexed property. Returns
    /// up to `k` entries sorted ascending by distance. Empty if the
    /// property isn't registered, no vectors are indexed, or the query
    /// dimension doesn't match.
    #[must_use]
    pub fn vector_search(
        &self,
        property_id: PropertyId,
        query: &[f32],
        k: usize,
        metric: Distance,
    ) -> Vec<(EntityId, f32)> {
        self.vector.search(property_id, query, k, metric)
    }

    /// Database directory path.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.db.path()
    }

    /// Active MANIFEST snapshot.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        self.db.manifest()
    }

    /// Memtable record count + size estimate, for flush threshold logic.
    #[must_use]
    pub fn memtable_stats(&self) -> (u64, u64) {
        (self.memtable.record_count(), self.memtable.size_bytes())
    }

    /// Start a write transaction. The returned [`WriteTxn`] holds an
    /// exclusive `&mut Engine` borrow — no other writes can happen until
    /// the transaction is committed or dropped.
    pub fn begin_write(&mut self) -> WriteTxn<'_> {
        let begin_snapshot = TxId::new(self.db.manifest().last_tx_id);
        let tx_id = TxId::new(self.db.allocate_tx_id());
        WriteTxn {
            engine: self,
            tx_id,
            pending: Vec::new(),
            isolation: IsolationLevel::default(),
            begin_snapshot,
            read_set: Vec::new(),
        }
    }

    /// MVCC point lookup across memtable + every open SSTable.
    ///
    /// Newest layer first: memtable, then SSTables in (level, descending
    /// file_seq) order. We gather all candidate versions for the UUID
    /// (across Entity / HyperEdge / Tombstone kinds) and feed them to the
    /// visibility resolver.
    pub fn snapshot_read(
        &mut self,
        uuid: &uuid::Uuid,
        snapshot: TxId,
    ) -> Result<Resolved<Record>, EngineError> {
        let mut candidates: Vec<Record> = Vec::new();

        // Memtable first.
        for kind in [
            crate::record::RecordKind::Entity,
            crate::record::RecordKind::HyperEdge,
            crate::record::RecordKind::Tombstone,
        ] {
            let key = SSTableKey {
                kind: kind.as_byte(),
                primary: uuid.as_bytes().to_vec(),
            };
            if let Some(vs) = self.memtable.versions(&key) {
                candidates.extend(vs.iter().cloned());
            }
        }

        // Each open SSTable.
        for sst in &mut self.sstables {
            for kind in [
                crate::record::RecordKind::Entity,
                crate::record::RecordKind::HyperEdge,
                crate::record::RecordKind::Tombstone,
            ] {
                let key = SSTableKey {
                    kind: kind.as_byte(),
                    primary: uuid.as_bytes().to_vec(),
                };
                // Iterate the SSTable; in v1 we don't have a block index so
                // find() linear-scans. For multi-version-per-key correctness
                // we collect every match (not just the first).
                let mut iter = sst.iter();
                for item in &mut iter {
                    let (rec, _) = item?;
                    let k = SSTableKey::for_record(&rec);
                    match k.cmp(&key) {
                        Ordering::Less => {}
                        Ordering::Equal => candidates.push(rec),
                        Ordering::Greater => break, // sorted file, past target
                    }
                }
            }
        }

        Ok(match resolve_iter(candidates.iter(), snapshot) {
            Resolved::Missing => Resolved::Missing,
            Resolved::Deleted { deleted_at } => Resolved::Deleted { deleted_at },
            Resolved::Live(r) => Resolved::Live(r.clone()),
        })
    }

    /// Number of open SSTables.
    #[must_use]
    pub fn sstable_count(&self) -> usize {
        self.sstables.len()
    }

    /// Find the most recent tx_id whose commit timestamp is at or before
    /// `timestamp_us` (microseconds since Unix epoch). Returns `None` if
    /// no such tx exists in the in-memory commit-timestamp map.
    ///
    /// v1 limitation: the map is in-memory only and lost on engine
    /// open/close. Only tx_ids committed during the current process
    /// lifetime are findable. v2 will persist timestamps.
    #[must_use]
    pub fn tx_at_or_before(&self, timestamp_us: i64) -> Option<TxId> {
        self.commit_timestamps
            .iter()
            .rev()
            .find(|(_, ts)| **ts <= timestamp_us)
            .map(|(tx, _)| *tx)
    }

    /// Record the wall-clock timestamp for a previously committed tx_id.
    /// Used by tests + bench mode to seed the map deterministically; in
    /// normal operation `WriteTxn::commit` populates it automatically.
    pub fn record_commit_timestamp(&mut self, tx_id: TxId, timestamp_us: i64) {
        self.commit_timestamps.insert(tx_id, timestamp_us);
    }

    /// Commit timestamp for a specific tx_id, if recorded in this session.
    #[must_use]
    pub fn commit_timestamp_us(&self, tx_id: TxId) -> Option<i64> {
        self.commit_timestamps.get(&tx_id).copied()
    }

    /// Configure the retention policy for a type. Future compactions will
    /// honour this when deciding how many superseded versions to keep.
    /// Records committed BEFORE this call are also subject to the new
    /// policy at the next compaction.
    ///
    /// The policy is also persisted via a `RetentionPolicyRecord` so it
    /// survives engine restarts (v2.0+). Falls back to in-memory-only if
    /// the WAL write fails (callers can retry).
    pub fn set_retention_policy(&mut self, type_id: TypeId, policy: RetentionPolicy) {
        self.retention.insert(type_id, policy);
        let (policy_kind, keep_last_n) = match policy {
            RetentionPolicy::LatestOnly => (0u8, 0u32),
            RetentionPolicy::Versioned { keep_last_n } => (1u8, keep_last_n),
            RetentionPolicy::Audited => (2u8, 0u32),
        };
        let rec = Record::RetentionPolicy(crate::record::RetentionPolicyRecord {
            type_id,
            policy_kind,
            keep_last_n,
        });
        // Best-effort durability: commit via an internal one-record txn.
        // Failure leaves the in-memory state correct but unpersisted —
        // matches the v1.3 contract.
        let mut txn = self.begin_write();
        txn.put_raw(rec);
        let _ = txn.commit();
    }

    /// Look up the retention policy for a type. Returns `LatestOnly`
    /// (the default) if no policy is set.
    #[must_use]
    pub fn retention_policy(&self, type_id: TypeId) -> RetentionPolicy {
        self.retention
            .get(&type_id)
            .copied()
            .unwrap_or(RetentionPolicy::LatestOnly)
    }

    /// Streaming variant of [`Self::snapshot_iter`] — lazily k-way-merges
    /// the memtable + open SSTables in `(kind, primary)` order. Yields
    /// one resolved record at a time without materialising the full
    /// result set; peak memory is O(sources × avg record size) instead
    /// of O(N).
    ///
    /// Use this for large-scan paths (`/iter`, `/query_stream`, the
    /// query executor) where the caller doesn't need random access.
    /// For backward compatibility, [`Self::snapshot_iter`] still
    /// materialises a `Vec` internally by collecting from this iterator.
    pub fn snapshot_iter_streaming(
        &self,
        snapshot: TxId,
    ) -> SnapshotStream<'_> {
        // Materialise memtable into an owned, sorted Vec. Memtable
        // is small relative to SSTables and already in memory; this
        // copy is the right cost-vs-complexity tradeoff for v2.0.
        let mem: Vec<(SSTableKey, Record)> = self
            .memtable
            .iter()
            .map(|(k, r)| (k.clone(), r.clone()))
            .collect();
        let mut sources: Vec<MergeSource<'_>> = Vec::with_capacity(self.sstables.len() + 1);
        sources.push(MergeSource::Memtable(mem.into_iter()));
        for sst in &self.sstables {
            sources.push(MergeSource::SSTable(sst.iter()));
        }
        SnapshotStream::new(sources, snapshot)
    }

    /// Iterate every record visible at `snapshot`, in (kind, primary)
    /// order, deduplicating across memtable + SSTables. Useful for scans.
    /// O(N) — v1 has no block index.
    ///
    /// Materialises the full result set in a `Vec`. For very large scans,
    /// prefer [`Self::snapshot_iter_streaming`] which yields records one
    /// at a time without buffering.
    pub fn snapshot_iter(&mut self, snapshot: TxId) -> Result<Vec<Record>, EngineError> {
        self.snapshot_iter_streaming(snapshot)
            .collect::<Result<Vec<_>, _>>()
    }

    /// EXPLAIN-style trace for a query. Runs the planner and returns one
    /// entry per pattern in planned order: original index, cardinality
    /// estimate at the moment of selection, a brief shape summary, and
    /// the binds-vs-uses split for variables.
    ///
    /// Side-effect-free; doesn't execute the query.
    #[must_use]
    pub fn explain_query(&self, req: &crate::wire_query::QueryRequest) -> Vec<crate::query::ExplainEntry> {
        crate::query::plan::explain(self, &req.patterns)
    }

    /// Drain the memtable into a new SSTable, update MANIFEST, rotate
    /// the WAL. Crash-safe sequence:
    ///
    /// 1. Allocate new SSTable file_seq.
    /// 2. Stream memtable into SSTableWriter → finish() (write-temp +
    ///    fsync + rename + fsync_dir).
    /// 3. Allocate new WAL file_seq + create the new .ndblog file.
    /// 4. Build a new MANIFEST: add the SSTable entry, set
    ///    active_wal_seq to the new WAL. Write + fsync + flip CURRENT.
    /// 5. Open SSTableReader on the new file; prepend to the
    ///    self.sstables chain.
    /// 6. Drop the old WAL file. (Optional — left on disk for safety
    ///    in v1 to keep recovery options open.)
    pub fn flush(&mut self) -> Result<(), EngineError> {
        if self.memtable.is_empty() {
            return Ok(());
        }
        // Step 1 + 2: write memtable to new SSTable.
        let sst_seq = self.db.allocate_file_seq();
        let sst_path = sstable_path(self.db.path(), sst_seq);
        let mut writer = SSTableWriter::create(&sst_path)?;
        self.memtable.flush_into(&mut writer)?;
        writer.finish()?;

        // Step 3: mint new WAL.
        let new_wal_seq = self.db.allocate_file_seq();
        let new_wal_path = wal_path(self.db.path(), new_wal_seq);
        let new_wal = WriteAheadLog::create(&new_wal_path)?;

        // Step 4: update MANIFEST + CURRENT.
        let mut manifest = self.db.manifest().clone();
        let old_wal_seq = manifest.active_wal_seq;
        manifest.sstables.push(ManifestEntry {
            file_seq: sst_seq,
            level: 0,
        });
        manifest.active_wal_seq = new_wal_seq;
        self.db.write_manifest(manifest)?;

        // Step 5: open the new SSTable reader and prepend it.
        let reader = SSTableReader::open(&sst_path)?;
        self.sstables.insert(0, reader);

        // Replace WAL.
        if let Some(old) = self.wal.replace(new_wal) {
            // best-effort close; if it errors, we can still proceed — the
            // file is no longer the active WAL.
            let _ = old.close();
        }

        // Step 6: remove the old WAL file. Safe because all its records
        // are now durable in the new SSTable.
        if old_wal_seq != 0 {
            let old = wal_path(self.db.path(), old_wal_seq);
            let _ = std::fs::remove_file(&old);
        }

        Ok(())
    }

    /// Full compaction — merge every open SSTable into one new SSTable at
    /// level 1. Drops records whose later version supersedes them, and
    /// drops the tombstone marker once it has done its job (i.e. when
    /// the latest visible "event" for that key is the tombstone).
    ///
    /// v1 simplifications:
    ///
    /// - **Full compaction only**, not tiered (no L0/L1/L2 levels yet —
    ///   the new SSTable lands at level 1; future tiered compaction will
    ///   rewrite this).
    /// - **No snapshot tracking.** Any in-flight read at a snapshot older
    ///   than the current MANIFEST `last_tx_id` may return Missing for
    ///   keys that previously resolved Live. Acceptable for v1 because
    ///   the engine is single-process and the caller can hold off
    ///   compaction during long reads. v2 will track the oldest live
    ///   snapshot and only drop versions older than it.
    /// - **Memtable is NOT flushed first.** Compaction operates only on
    ///   on-disk SSTables; the memtable continues to serve writes during
    ///   compaction. (Compaction is short relative to memtable lifetime
    ///   in practice.)
    ///
    /// Steps:
    /// 1. If <2 SSTables, no-op.
    /// 2. Stream every record from every input SSTable into a single
    ///    `BTreeMap<SSTableKey, Vec<Record>>`.
    /// 3. For each key, run `resolve_iter(_, TxId::ACTIVE)` to find the
    ///    visible winner. If `Live`, emit it. If `Deleted`, drop the
    ///    whole key (and its tombstone).
    /// 4. Stream survivors into a new SSTable via `SSTableWriter`. Finish
    ///    publishes atomically.
    /// 5. Update MANIFEST: replace `sstables` with `[new_entry]`, leave
    ///    `active_wal_seq` and `last_tx_id` alone.
    /// 6. Open the new SSTable reader and replace `self.sstables`.
    /// 7. Delete the old SSTable files (best-effort).
    ///
    /// Equivalent to `compact_with_floor(TxId::ACTIVE)` — drops every
    /// superseded version. For snapshot-aware compaction that protects
    /// in-flight readers, use [`Self::compact_with_floor`] with the
    /// oldest active snapshot tx_id.
    pub fn compact(&mut self) -> Result<CompactionStats, EngineError> {
        self.compact_with_floor(TxId::ACTIVE)
    }

    /// Snapshot-aware compaction: drop a superseded version V only if
    /// it was superseded BEFORE `oldest_active_snapshot`. Versions
    /// superseded at-or-after that tx are still required by some active
    /// reader; the compactor keeps them.
    ///
    /// `oldest_active_snapshot = TxId::ACTIVE` is the v1.3 baseline —
    /// no active reader is registered, drop everything superseded.
    ///
    /// For `RetentionPolicy::Audited` the snapshot floor is irrelevant
    /// (every version is kept anyway). For `Versioned { keep_last_n }`
    /// the floor takes precedence: a version that's "old enough" by N
    /// but still needed by a snapshot will be retained.
    pub fn compact_with_floor(
        &mut self,
        oldest_active_snapshot: TxId,
    ) -> Result<CompactionStats, EngineError> {
        if self.sstables.len() < 2 {
            // Single (or zero) SSTable: no merge to perform. We could
            // still drop tombstones, but for v1 the cost is not worth
            // the complexity — wait for a real flush to accumulate
            // multiple SSTables.
            return Ok(CompactionStats {
                records_in: 0,
                records_out: 0,
                sstables_in: self.sstables.len(),
                new_sstable_seq: None,
            });
        }

        // Step 2: collect by key + build the cross-bucket "killed" map.
        //
        // Entities and tombstones for the same UUID sort to different
        // SSTableKey buckets (kind byte differs). To drop a tombstoned
        // entity AND its tombstone, we need to consult tombstone
        // information across buckets. Build a `killed: uuid → max
        // tombstone tx_id_supersede` map during the first pass; emit
        // phase consults it.
        let mut by_key: BTreeMap<SSTableKey, Vec<Record>> = BTreeMap::new();
        let mut killed: HashMap<uuid::Uuid, TxId> = HashMap::new();
        let mut records_in: u64 = 0;
        for sst in &mut self.sstables {
            for item in sst.iter() {
                let (rec, _) = item?;
                records_in += 1;
                if let Record::Tombstone(t) = &rec {
                    let entry = killed.entry(t.target_id).or_insert(t.tx_id_supersede);
                    if t.tx_id_supersede > *entry {
                        *entry = t.tx_id_supersede;
                    }
                }
                let k = SSTableKey::for_record(&rec);
                by_key.entry(k).or_default().push(rec);
            }
        }

        // Step 3 + 4: resolve per-key, drop tombstoned entities and the
        // tombstones themselves (v1: no snapshot tracking), write
        // survivors.
        let new_seq = self.db.allocate_file_seq();
        let new_path = sstable_path(self.db.path(), new_seq);
        let mut writer = SSTableWriter::create(&new_path)?;
        let mut records_out: u64 = 0;
        for (_k, versions) in by_key {
            // Per-type retention policy decides how many versions to
            // keep. Default LatestOnly preserves the historical v1
            // behaviour for types with no explicit policy.
            let type_id = versions.iter().find_map(|r| match r {
                Record::Entity(e) => Some(e.type_id),
                Record::HyperEdge(h) => Some(h.type_id),
                _ => None,
            });
            let policy = type_id
                .map(|t| self.retention_policy(t))
                .unwrap_or_default();
            match policy {
                RetentionPolicy::LatestOnly => {
                    if oldest_active_snapshot == TxId::ACTIVE {
                        // Fast path: no snapshot floor — current v1.3 behaviour.
                        emit_latest_only(&mut writer, &versions, &killed, &mut records_out)?;
                    } else {
                        emit_latest_only_with_floor(
                            &mut writer,
                            &versions,
                            &killed,
                            oldest_active_snapshot,
                            &mut records_out,
                        )?;
                    }
                }
                RetentionPolicy::Audited => {
                    // Preserve every record (including tombstones) — full audit trail.
                    for r in &versions {
                        writer.append(r)?;
                        records_out += 1;
                    }
                }
                RetentionPolicy::Versioned { keep_last_n } => {
                    emit_versioned(
                        &mut writer,
                        versions,
                        keep_last_n.max(1) as usize,
                        &mut records_out,
                    )?;
                }
            }
        }
        let _footer = writer.finish()?;

        // Step 5: MANIFEST update — replace sstables entirely.
        let old_sstable_seqs: Vec<u64> = self
            .db
            .manifest()
            .sstables
            .iter()
            .map(|e| e.file_seq)
            .collect();
        let sstables_in = old_sstable_seqs.len();
        let mut manifest = self.db.manifest().clone();
        manifest.sstables = vec![ManifestEntry {
            file_seq: new_seq,
            level: 1,
        }];
        self.db.write_manifest(manifest)?;

        // Step 6: re-open SSTable readers from the (now single) new entry.
        let reader = SSTableReader::open(&new_path)?;
        self.sstables.clear();
        self.sstables.push(reader);

        // Step 7: remove old files (best-effort). Also remove the
        // companion `<seq>.idx` block-index sidecar if it exists.
        for old_seq in old_sstable_seqs {
            let p = sstable_path(self.db.path(), old_seq);
            let _ = std::fs::remove_file(&p);
            let _ = std::fs::remove_file(crate::block_index::sidecar_path_for(&p));
        }

        // Rebuild indexes since we dropped tombstoned records.
        self.rebuild_indexes()?;

        Ok(CompactionStats {
            records_in,
            records_out,
            sstables_in,
            new_sstable_seq: Some(new_seq),
        })
    }

    /// `fsync` + release LOCK.
    pub fn close(mut self) -> Result<(), EngineError> {
        if let Some(wal) = self.wal.take() {
            wal.close()?;
        }
        self.db.close()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WriteTxn
// ---------------------------------------------------------------------------

/// A write transaction. Buffers records in memory; nothing touches disk
/// until [`commit`](Self::commit). Dropping without calling commit is a
/// rollback (records are discarded; the allocated `TxId` becomes a gap in
/// the sequence, which the engine tolerates).
#[derive(Debug)]
pub struct WriteTxn<'a> {
    engine: &'a mut Engine,
    tx_id: TxId,
    pending: Vec<Record>,
    isolation: IsolationLevel,
    /// Snapshot tx_id this transaction sees. Defaults to
    /// `engine.manifest().last_tx_id` at begin_write time.
    begin_snapshot: TxId,
    /// Reads performed via [`WriteTxn::read`] for serializable-level
    /// conflict detection. Empty for `SnapshotIsolation`. Each entry is
    /// `(key, snapshot_at_read)`.
    read_set: Vec<(uuid::Uuid, TxId)>,
}

impl WriteTxn<'_> {
    /// `TxId` allocated for this transaction.
    #[must_use]
    pub fn tx_id(&self) -> TxId {
        self.tx_id
    }

    /// Snapshot tx_id this transaction sees. Reads via [`Self::read`]
    /// resolve at this snapshot.
    #[must_use]
    pub fn snapshot(&self) -> TxId {
        self.begin_snapshot
    }

    /// Switch to a different isolation level. Default is
    /// `SnapshotIsolation`. For multi-key invariants pass
    /// `IsolationLevel::Serializable` — the engine tracks reads done
    /// via [`Self::read`] and aborts the commit if a later transaction
    /// modified any of those keys (the check is structurally trivial in
    /// v1 single-writer mode; see [`IsolationLevel::Serializable`] docs).
    #[must_use]
    pub fn with_isolation(mut self, level: IsolationLevel) -> Self {
        self.isolation = level;
        self
    }

    /// Snapshot read at the transaction's begin snapshot. Used by
    /// serializable transactions to track the read set; for snapshot
    /// isolation the call is equivalent to
    /// `engine.snapshot_read(uuid, txn.snapshot())` without the bookkeeping.
    pub fn read(&mut self, uuid: &uuid::Uuid) -> Result<Resolved<Record>, EngineError> {
        let result = self.engine.snapshot_read(uuid, self.begin_snapshot)?;
        if matches!(self.isolation, IsolationLevel::Serializable) {
            self.read_set.push((*uuid, self.begin_snapshot));
        }
        Ok(result)
    }

    /// Push an entity record. The transaction stamps `tx_id_assert` for
    /// you — pass the record with any value (it will be overwritten).
    pub fn put_entity(&mut self, mut record: EntityRecord) {
        record.tx_id_assert = self.tx_id;
        record.tx_id_supersede = TxId::new(TX_ACTIVE);
        self.pending.push(Record::Entity(record));
    }

    /// Push a hyperedge record. Transaction stamps `tx_id_assert`.
    pub fn put_hyperedge(&mut self, mut record: HyperEdgeRecord) {
        record.tx_id_assert = self.tx_id;
        record.tx_id_supersede = TxId::new(TX_ACTIVE);
        self.pending.push(Record::HyperEdge(record));
    }

    /// Push a tombstone for an entity or hyperedge. Transaction stamps
    /// `tx_id_supersede`.
    pub fn delete(&mut self, target: uuid::Uuid) {
        self.pending.push(Record::Tombstone(TombstoneRecord {
            target_id: target,
            tx_id_supersede: self.tx_id,
        }));
    }

    /// Push a raw record. Any tx-stamping the caller did is preserved;
    /// used by replay paths.
    pub fn put_raw(&mut self, record: Record) {
        self.pending.push(record);
    }

    /// Append every pending record to the WAL, `fsync_data`, then insert
    /// into the memtable. On any error before fsync, the records are
    /// effectively rolled back (nothing durable was written). On error
    /// after fsync, the WAL has the records but the memtable doesn't —
    /// recovery on the next open will replay them.
    pub fn commit(mut self) -> Result<TxId, EngineError> {
        if self.pending.is_empty() {
            return Ok(self.tx_id);
        }
        // Serializable Snapshot Isolation conflict check: for each key
        // the txn read, verify no later-committed tx has modified it.
        // In v1's single-writer model this is structurally trivial — no
        // other writer could have committed during this txn's lifetime
        // (`&mut Engine` guarantees serial writes). The check is
        // shipped here so the API contract holds for v2 multi-writer.
        if matches!(self.isolation, IsolationLevel::Serializable) {
            let read_set = std::mem::take(&mut self.read_set);
            for (key, snap) in read_set {
                if let Resolved::Live(r) = self.engine.snapshot_read(&key, TxId::ACTIVE)? {
                    let modified_tx = match &r {
                        Record::Entity(e) => e.tx_id_assert.get(),
                        Record::HyperEdge(h) => h.tx_id_assert.get(),
                        _ => 0,
                    };
                    if modified_tx > snap.get() {
                        return Err(EngineError::SerializationFailure {
                            key,
                            read_at: snap.get(),
                            modified_at: modified_tx,
                        });
                    }
                }
            }
        }
        // Validate every record FIRST. Validation failure aborts the
        // transaction cleanly — nothing reaches the WAL, no partial
        // state.
        for r in &self.pending {
            self.engine.validation.check(r)?;
        }
        let wal = self
            .engine
            .wal
            .as_mut()
            .expect("WAL active during commit (engine open invariant)");
        // Record the wall-clock commit timestamp as a durable record so
        // `as of "<rfc3339>"` queries survive engine restart (v2.0+).
        // Computed once here so the same value goes to WAL + memtable
        // + in-memory map.
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
        let mut records: Vec<Record> = self.pending;
        records.push(Record::TxTimestamp(crate::record::TxTimestampRecord {
            tx_id: self.tx_id,
            timestamp_us: now_us,
        }));
        wal.append_batch(&records)?;
        wal.sync()?;
        // Memtable insert + index update happen AFTER WAL durability so a
        // crash before this point cleanly rolls back the transaction; a
        // crash AFTER WAL durability means the records are durable in the
        // log and will be replayed on the next open (which will repopulate
        // the in-memory state).
        for r in records {
            // Side-effects of metadata records: keep in-memory maps in
            // sync with what was just durably written.
            match &r {
                Record::TxTimestamp(t) => {
                    self.engine.commit_timestamps.insert(t.tx_id, t.timestamp_us);
                }
                Record::RetentionPolicy(rp) => {
                    if let Some(p) = decode_retention_policy(rp.policy_kind, rp.keep_last_n) {
                        self.engine.retention.insert(rp.type_id, p);
                    }
                }
                _ => {}
            }
            self.engine.lookup_key.apply(&r, self.tx_id);
            self.engine.adjacency.apply(&r, self.tx_id);
            self.engine.type_cluster.apply(&r, self.tx_id);
            self.engine.vector.apply(&r, self.tx_id);
            self.engine.property_btree.apply(&r, self.tx_id);
            self.engine.memtable.insert(r)?;
        }
        Ok(self.tx_id)
    }

    /// Discard the transaction. Pending records are dropped; no WAL,
    /// no memtable mutation. The allocated `TxId` becomes a gap.
    pub fn rollback(self) {
        drop(self.pending);
    }
}

// ---------------------------------------------------------------------------
// Path helpers + recovery
// ---------------------------------------------------------------------------

/// Emit just the snapshot-visible winner (current LatestOnly behaviour).
/// Drops the whole key if a tombstone in `killed` supersedes it.
fn emit_latest_only(
    writer: &mut SSTableWriter,
    versions: &[Record],
    killed: &HashMap<uuid::Uuid, TxId>,
    records_out: &mut u64,
) -> Result<(), EngineError> {
    match crate::mvcc::resolve_iter(versions.iter(), TxId::ACTIVE) {
        crate::mvcc::Resolved::Missing | crate::mvcc::Resolved::Deleted { .. } => Ok(()),
        crate::mvcc::Resolved::Live(winner) => {
            let (uuid, winner_tx) = match winner {
                Record::Entity(e) => (Some(e.entity_id.into_uuid()), e.tx_id_assert),
                Record::HyperEdge(h) => (Some(h.hyperedge_id.into_uuid()), h.tx_id_assert),
                _ => (None, TxId::new(0)),
            };
            if let Some(u) = uuid
                && let Some(killed_at) = killed.get(&u)
                && killed_at.get() >= winner_tx.get()
            {
                return Ok(());
            }
            writer.append(winner)?;
            *records_out += 1;
            Ok(())
        }
    }
}

/// Snapshot-aware LatestOnly: emit every version that some live reader
/// (snapshot ≥ `oldest_active_snapshot`) might still observe. Drops
/// only versions fully shadowed at + after the floor.
///
/// A version V with assert tx `a_i` is observable at snapshot T iff
/// `a_i ≤ T` and no later version `V'` has `a_{i+1} ≤ T`. So V is needed
/// iff there exists T in `[oldest_active_snapshot, ACTIVE]` such that
/// V is observable at T → iff the next version's assert > floor (or V
/// is the last version, trivially live at ACTIVE).
fn emit_latest_only_with_floor(
    writer: &mut SSTableWriter,
    versions: &[Record],
    killed: &HashMap<uuid::Uuid, TxId>,
    oldest_active_snapshot: TxId,
    records_out: &mut u64,
) -> Result<(), EngineError> {
    // Sort ascending by assert tx (versions arriving from SSTable scan
    // are in unspecified order across files; explicit sort is cheap and
    // makes the live-interval logic unambiguous).
    let mut sorted: Vec<&Record> = versions.iter().collect();
    sorted.sort_by_key(|r| match r {
        Record::Entity(e) => e.tx_id_assert.get(),
        Record::HyperEdge(h) => h.tx_id_assert.get(),
        Record::Tombstone(t) => t.tx_id_supersede.get(),
        _ => 0,
    });
    let floor = oldest_active_snapshot.get();
    for i in 0..sorted.len() {
        let next_tx = sorted.get(i + 1).map(|r| match r {
            Record::Entity(e) => e.tx_id_assert.get(),
            Record::HyperEdge(h) => h.tx_id_assert.get(),
            Record::Tombstone(t) => t.tx_id_supersede.get(),
            _ => u64::MAX,
        });
        let keep = match next_tx {
            None => true,                    // last version — live at ACTIVE
            Some(n) => n > floor,            // a reader at floor still sees this
        };
        if !keep {
            continue;
        }
        // Cross-bucket tombstone check (only for non-tombstone records).
        let (uuid, tx) = match sorted[i] {
            Record::Entity(e) => (Some(e.entity_id.into_uuid()), e.tx_id_assert),
            Record::HyperEdge(h) => (Some(h.hyperedge_id.into_uuid()), h.tx_id_assert),
            _ => (None, TxId::new(0)),
        };
        if let Some(u) = uuid
            && let Some(killed_at) = killed.get(&u)
            && killed_at.get() >= tx.get()
            && killed_at.get() <= floor
        {
            // Tombstone fully retired before any live snapshot.
            continue;
        }
        writer.append(sorted[i])?;
        *records_out += 1;
    }
    Ok(())
}

/// Emit the N most-recent versions for a `Versioned { keep_last_n }`
/// policy. Sort by `tx_id_assert` descending; take the first N. Tombstones
/// stack alongside the version chain — they may be retained too if they
/// fall in the N latest by tx_id_supersede.
fn emit_versioned(
    writer: &mut SSTableWriter,
    mut versions: Vec<Record>,
    n: usize,
    records_out: &mut u64,
) -> Result<(), EngineError> {
    versions.sort_by_key(|r| {
        std::cmp::Reverse(match r {
            Record::Entity(e) => e.tx_id_assert.get(),
            Record::HyperEdge(h) => h.tx_id_assert.get(),
            Record::Tombstone(t) => t.tx_id_supersede.get(),
            _ => 0,
        })
    });
    for r in versions.iter().take(n) {
        writer.append(r)?;
        *records_out += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Lazy snapshot iterator (v2.0+) — k-way merge memtable + SSTables
// ---------------------------------------------------------------------------

/// One source in the k-way merge. Either the memtable's pre-collected
/// vec or an SSTable's mmap-backed iterator.
enum MergeSource<'a> {
    Memtable(std::vec::IntoIter<(SSTableKey, Record)>),
    SSTable(crate::sstable::SSTableIter<'a>),
}

impl MergeSource<'_> {
    fn next_item(&mut self) -> Result<Option<(SSTableKey, Record)>, EngineError> {
        match self {
            Self::Memtable(it) => Ok(it.next()),
            Self::SSTable(it) => match it.next() {
                None => Ok(None),
                Some(Ok((rec, _))) => Ok(Some((SSTableKey::for_record(&rec), rec))),
                Some(Err(e)) => Err(EngineError::SSTable(e)),
            },
        }
    }
}

/// Streaming snapshot iterator. Holds owned merge state plus an
/// immutable borrow of the SSTable readers, so the engine remains
/// readable concurrently (relevant once v2.0's RwLock relaxation lands).
pub struct SnapshotStream<'a> {
    sources: Vec<MergeSource<'a>>,
    /// Current head of each source. `None` when that source is exhausted.
    heads: Vec<Option<(SSTableKey, Record)>>,
    snapshot: TxId,
    primed: bool,
    /// Error captured during merge; subsequent next() calls return None.
    errored: bool,
}

impl<'a> SnapshotStream<'a> {
    fn new(sources: Vec<MergeSource<'a>>, snapshot: TxId) -> Self {
        let heads = (0..sources.len()).map(|_| None).collect();
        Self {
            sources,
            heads,
            snapshot,
            primed: false,
            errored: false,
        }
    }

    fn prime(&mut self) -> Result<(), EngineError> {
        for (i, src) in self.sources.iter_mut().enumerate() {
            self.heads[i] = src.next_item()?;
        }
        self.primed = true;
        Ok(())
    }

    /// Pull the next visible record. Returns `Ok(None)` at end of stream.
    fn pump(&mut self) -> Result<Option<Record>, EngineError> {
        if !self.primed {
            self.prime()?;
        }
        loop {
            // Find the smallest head key across all sources.
            let mut smallest: Option<SSTableKey> = None;
            for h in &self.heads {
                if let Some((k, _)) = h
                    && smallest.as_ref().is_none_or(|s| k < s)
                {
                    smallest = Some(k.clone());
                }
            }
            let Some(target) = smallest else {
                return Ok(None); // all sources exhausted
            };
            // Collect all records with this key + advance those sources.
            let mut versions: Vec<Record> = Vec::new();
            for i in 0..self.sources.len() {
                while let Some((k, _)) = &self.heads[i] {
                    if *k != target {
                        break;
                    }
                    let (_, rec) = self.heads[i].take().expect("head present");
                    versions.push(rec);
                    self.heads[i] = self.sources[i].next_item()?;
                }
            }
            // Resolve visible winner for this key at the requested snapshot.
            if let Some(r) = crate::mvcc::resolve_iter(versions.iter(), self.snapshot).into_live()
                && crate::mvcc::visible_at(r, self.snapshot)
            {
                return Ok(Some(r.clone()));
            }
            // Else: key was tombstoned at this snapshot — keep pumping.
        }
    }
}

impl Iterator for SnapshotStream<'_> {
    type Item = Result<Record, EngineError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.errored {
            return None;
        }
        match self.pump() {
            Ok(Some(r)) => Some(Ok(r)),
            Ok(None) => None,
            Err(e) => {
                self.errored = true;
                Some(Err(e))
            }
        }
    }
}

/// Map a `RetentionPolicyRecord` (policy_kind, keep_last_n) into the
/// typed `RetentionPolicy`. Returns `None` for unknown policy_kind so
/// future kinds added in v2.1+ don't break v2.0 readers.
fn decode_retention_policy(policy_kind: u8, keep_last_n: u32) -> Option<RetentionPolicy> {
    match policy_kind {
        0 => Some(RetentionPolicy::LatestOnly),
        1 => Some(RetentionPolicy::Versioned { keep_last_n }),
        2 => Some(RetentionPolicy::Audited),
        _ => None,
    }
}

fn wal_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{seq:06}{WAL_FILENAME_SUFFIX}"))
}

fn sstable_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{seq:06}{SSTABLE_FILENAME_SUFFIX}"))
}

/// Replay every clean WAL record into the memtable. Returns the safe
/// truncate boundary AND the maximum `effective_tx` seen during replay.
///
/// The max-tx return is critical: the previous MANIFEST's `last_tx_id` was
/// persisted at the last flush. Any commits since then are in the WAL but
/// not the MANIFEST. Without reconciling, a snapshot read at
/// `manifest.last_tx_id` would treat the replayed records as invisible.
fn replay_wal_into(path: &Path, memtable: &mut Memtable) -> Result<(u64, u64), EngineError> {
    if !path.exists() {
        return Ok((0, 0));
    }
    let mut reader = WalReader::open(path)?;
    let mut max_tx: u64 = 0;
    while let Some((rec, _lsn)) = reader.next_record()? {
        let tx = match &rec {
            Record::Entity(e) => e.tx_id_assert.get(),
            Record::HyperEdge(h) => h.tx_id_assert.get(),
            Record::Tombstone(t) => t.tx_id_supersede.get(),
            Record::TxTimestamp(t) => t.tx_id.get(),
            Record::TypeName(_)
            | Record::RoleName(_)
            | Record::PropertyKey(_)
            | Record::RetentionPolicy(_) => 0,
        };
        if tx > max_tx {
            max_tx = tx;
        }
        memtable.insert(rec)?;
    }
    Ok((reader.pos(), max_tx))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{EntityId, HyperedgeId, PropertyId, RoleId, TypeId};
    use crate::record::{EntityRecord, HyperEdgeRecord};
    use crate::value::Value;

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ndb-engine-{}-{}",
            name,
            uuid::Uuid::now_v7().simple()
        ));
        p
    }

    fn make_entity(eid: EntityId, prop: &str) -> EntityRecord {
        EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0), // overwritten by WriteTxn
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(1), Value::String(prop.into()))],
        }
    }

    #[test]
    fn create_write_read_basic() {
        let dir = temp_dir("create_write_read");
        let mut engine = Engine::create(&dir).unwrap();
        let eid = EntityId::now_v7();

        let mut txn = engine.begin_write();
        txn.put_entity(make_entity(eid, "alice"));
        let tx_id = txn.commit().unwrap();
        assert!(tx_id.get() > 0);

        let resolved = engine.snapshot_read(&eid.into_uuid(), tx_id).unwrap();
        match resolved {
            Resolved::Live(Record::Entity(e)) => {
                assert_eq!(e.entity_id, eid);
                assert_eq!(e.tx_id_assert, tx_id);
                match &e.properties[0].1 {
                    Value::String(s) => assert_eq!(s, "alice"),
                    other => panic!("wrong property: {other:?}"),
                }
            }
            other => panic!("expected Live(Entity), got {other:?}"),
        }
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restart_replays_wal() {
        let dir = temp_dir("restart_replay");
        let eid = EntityId::now_v7();
        let committed_tx;
        {
            let mut engine = Engine::create(&dir).unwrap();
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(eid, "bob"));
            committed_tx = txn.commit().unwrap();
            // Don't flush — leave the record in the WAL only.
            engine.close().unwrap();
        }
        // Reopen: WAL replay must restore the entity.
        let mut engine = Engine::open(&dir).unwrap();
        assert_eq!(engine.sstable_count(), 0);
        let resolved = engine
            .snapshot_read(&eid.into_uuid(), committed_tx)
            .unwrap();
        match resolved {
            Resolved::Live(Record::Entity(e)) => {
                assert_eq!(e.entity_id, eid);
                assert_eq!(e.tx_id_assert, committed_tx);
            }
            other => panic!("expected Live after WAL replay, got {other:?}"),
        }
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flush_close_reopen_lookup_each() {
        // Regression test for the MANIFEST staleness bug: after a flush
        // followed by more commits, close + reopen left manifest.last_tx_id
        // at the flush-time value. Records committed since then were in
        // the WAL but invisible at any snapshot ≤ last_tx_id.
        let dir = temp_dir("flush_close_reopen");
        let mut entities = Vec::new();
        {
            let mut engine = Engine::create(&dir).unwrap();
            for i in 0..30 {
                let mut txn = engine.begin_write();
                let eid = EntityId::now_v7();
                entities.push(eid);
                txn.put_entity(make_entity(eid, &format!("e-{i}")));
                txn.commit().unwrap();
                if i == 25 {
                    engine.flush().unwrap();
                }
            }
            engine.close().unwrap();
        }
        let mut engine = Engine::open(&dir).unwrap();
        // After reopen the WAL-replay reconciliation must have advanced
        // last_tx_id past the post-flush commits.
        let snap = TxId::new(engine.manifest().last_tx_id);
        assert!(
            snap.get() >= 30,
            "last_tx_id reconciliation failed: {snap:?}"
        );
        for (i, eid) in entities.iter().enumerate() {
            match engine.snapshot_read(&eid.into_uuid(), snap).unwrap() {
                Resolved::Live(Record::Entity(e)) => assert_eq!(&e.entity_id, eid, "i={i}"),
                other => panic!("i={i} eid={eid:?}: {other:?}"),
            }
        }
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flush_then_lookup_each_of_many_entities() {
        // Tightly matches the failing end_to_end loop but without the
        // hyperedges / restart noise. Writes 30 entities, flushes after
        // the 26th, then looks up every entity. All must be Live.
        let dir = temp_dir("flush_lookup_many");
        let mut engine = Engine::create(&dir).unwrap();
        let mut entities = Vec::new();
        for i in 0..30 {
            let mut txn = engine.begin_write();
            let eid = EntityId::now_v7();
            entities.push(eid);
            txn.put_entity(make_entity(eid, &format!("e-{i}")));
            txn.commit().unwrap();
            if i == 25 {
                engine.flush().unwrap();
            }
        }
        let snap = TxId::new(engine.manifest().last_tx_id);
        for (i, eid) in entities.iter().enumerate() {
            match engine.snapshot_read(&eid.into_uuid(), snap).unwrap() {
                Resolved::Live(Record::Entity(e)) => assert_eq!(&e.entity_id, eid, "i={i}"),
                other => panic!("i={i} eid={eid:?}: {other:?}"),
            }
        }
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flush_promotes_to_sstable_and_rotates_wal() {
        let dir = temp_dir("flush_rotate");
        let eid = EntityId::now_v7();
        let tx;
        {
            let mut engine = Engine::create(&dir).unwrap();
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(eid, "carol"));
            tx = txn.commit().unwrap();
            assert_eq!(engine.sstable_count(), 0);
            engine.flush().unwrap();
            assert_eq!(engine.sstable_count(), 1);
            // After flush the memtable is drained.
            assert_eq!(engine.memtable_stats().0, 0);
            // The entity is still readable via the new SSTable.
            match engine.snapshot_read(&eid.into_uuid(), tx).unwrap() {
                Resolved::Live(Record::Entity(e)) => assert_eq!(e.entity_id, eid),
                other => panic!("post-flush read: {other:?}"),
            }
            engine.close().unwrap();
        }
        // Reopen and confirm the SSTable shows up + the record is still
        // visible — but the WAL is the fresh rotated one (empty).
        let mut engine = Engine::open(&dir).unwrap();
        assert_eq!(engine.sstable_count(), 1);
        assert_eq!(engine.memtable_stats().0, 0);
        match engine.snapshot_read(&eid.into_uuid(), tx).unwrap() {
            Resolved::Live(Record::Entity(e)) => assert_eq!(e.entity_id, eid),
            other => panic!("post-restart read: {other:?}"),
        }
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn validation_rejects_missing_required_property_at_commit() {
        let dir = temp_dir("val_required");
        let mut engine = Engine::create(&dir).unwrap();
        engine.require_property(TypeId::new(1), PropertyId::new(7));
        let mut txn = engine.begin_write();
        // Entity of type 1 missing required property 7.
        txn.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(99), Value::I64(0))],
        });
        let err = txn.commit().unwrap_err();
        assert!(matches!(
            err,
            EngineError::Validation(ValidationError::MissingRequiredProperty { .. })
        ));
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn validation_rejects_wrong_value_tag() {
        let dir = temp_dir("val_tag");
        let mut engine = Engine::create(&dir).unwrap();
        engine.expect_value_tag(TypeId::new(1), PropertyId::new(7), crate::value::TAG_STRING);
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(7), Value::I64(42))],
        });
        let err = txn.commit().unwrap_err();
        assert!(matches!(
            err,
            EngineError::Validation(ValidationError::WrongValueTag { .. })
        ));
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn validation_aborts_atomically_no_records_written() {
        let dir = temp_dir("val_atomic");
        let mut engine = Engine::create(&dir).unwrap();
        engine.require_property(TypeId::new(1), PropertyId::new(7));
        // Push one good record AND one bad record in the same tx.
        let good_eid = EntityId::now_v7();
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: good_eid,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(7), Value::String("ok".into()))],
        });
        txn.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![], // missing required 7
        });
        let err = txn.commit().unwrap_err();
        assert!(matches!(err, EngineError::Validation(_)));
        // Even the GOOD record must not be in the engine — atomic
        // validation aborts the whole transaction before WAL append.
        let snap_after = TxId::new(engine.manifest().last_tx_id);
        assert!(matches!(
            engine
                .snapshot_read(&good_eid.into_uuid(), snap_after)
                .unwrap(),
            Resolved::Missing
        ));
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compaction_merges_sstables_into_one() {
        let dir = temp_dir("compact_merge");
        let mut engine = Engine::create(&dir).unwrap();
        // Three flushes → three SSTables at level 0.
        for batch in 0..3 {
            for _ in 0..5 {
                let mut txn = engine.begin_write();
                txn.put_entity(make_entity(EntityId::now_v7(), &format!("b{batch}")));
                txn.commit().unwrap();
            }
            engine.flush().unwrap();
        }
        assert_eq!(engine.sstable_count(), 3);
        let stats = engine.compact().unwrap();
        assert_eq!(stats.sstables_in, 3);
        assert!(stats.new_sstable_seq.is_some());
        // 15 entity commits + 15 durable TxTimestamp records (v2.0+) = 30.
        // All survive compaction (entities aren't superseded; timestamps
        // are append-only audit records with unique tx_ids).
        assert_eq!(stats.records_in, 30);
        assert_eq!(stats.records_out, 30);
        assert_eq!(engine.sstable_count(), 1);
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compaction_drops_superseded_versions() {
        let dir = temp_dir("compact_supersede");
        let mut engine = Engine::create(&dir).unwrap();
        let eid = EntityId::now_v7();
        // 5 versions of the same entity across 2 flushes.
        for i in 0..3 {
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(eid, &format!("v{i}")));
            txn.commit().unwrap();
        }
        engine.flush().unwrap();
        for i in 3..5 {
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(eid, &format!("v{i}")));
            txn.commit().unwrap();
        }
        engine.flush().unwrap();
        assert_eq!(engine.sstable_count(), 2);
        let stats = engine.compact().unwrap();
        // 5 entity commits + 5 TxTimestamps = 10 in. 1 surviving entity
        // + 5 TxTimestamps = 6 out.
        assert_eq!(stats.records_in, 10);
        assert_eq!(stats.records_out, 6);
        // Latest version still readable.
        let snap = TxId::new(engine.manifest().last_tx_id);
        match engine.snapshot_read(&eid.into_uuid(), snap).unwrap() {
            Resolved::Live(Record::Entity(e)) => {
                if let Value::String(s) = &e.properties[0].1 {
                    assert_eq!(s, "v4");
                } else {
                    panic!("wrong property type");
                }
            }
            other => panic!("post-compact read: {other:?}"),
        }
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compaction_drops_tombstoned_entities() {
        let dir = temp_dir("compact_tomb");
        let mut engine = Engine::create(&dir).unwrap();
        let eid = EntityId::now_v7();
        let alive_id = EntityId::now_v7();
        // Flush 1: entity + tombstone for it, plus a live unrelated entity.
        let mut txn = engine.begin_write();
        txn.put_entity(make_entity(eid, "doomed"));
        txn.commit().unwrap();
        engine.flush().unwrap();
        let mut txn = engine.begin_write();
        txn.delete(eid.into_uuid());
        txn.put_entity(make_entity(alive_id, "survivor"));
        txn.commit().unwrap();
        engine.flush().unwrap();
        assert_eq!(engine.sstable_count(), 2);
        let stats = engine.compact().unwrap();
        // 2 commits → 1 entity + (1 tombstone + 1 entity) = 3 user records
        // + 2 durable TxTimestamps (v2.0+, one per commit) = 5 in.
        // 1 surviving entity + 2 TxTimestamps = 3 out (tombstone + doomed
        // dropped; timestamps are append-only audit, distinct tx_ids).
        assert_eq!(stats.records_in, 5);
        assert_eq!(stats.records_out, 3);
        // Tombstoned entity gone after compaction.
        let snap = TxId::new(engine.manifest().last_tx_id);
        assert!(matches!(
            engine.snapshot_read(&eid.into_uuid(), snap).unwrap(),
            Resolved::Missing
        ));
        // Survivor still here.
        assert!(matches!(
            engine.snapshot_read(&alive_id.into_uuid(), snap).unwrap(),
            Resolved::Live(_)
        ));
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compaction_noop_when_single_sstable() {
        let dir = temp_dir("compact_noop");
        let mut engine = Engine::create(&dir).unwrap();
        let mut txn = engine.begin_write();
        txn.put_entity(make_entity(EntityId::now_v7(), "x"));
        txn.commit().unwrap();
        engine.flush().unwrap();
        let stats = engine.compact().unwrap();
        assert!(stats.new_sstable_seq.is_none());
        assert_eq!(stats.records_in, 0);
        assert_eq!(engine.sstable_count(), 1);
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lookup_by_external_key_round_trip() {
        let dir = temp_dir("lookup_key_e2e");
        let mut engine = Engine::create(&dir).unwrap();
        let email_prop = PropertyId::new(7);
        engine.register_lookup_key(email_prop);
        let eid = EntityId::now_v7();
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(email_prop, Value::String("alice@example.com".into()))],
        });
        txn.commit().unwrap();
        assert_eq!(
            engine.lookup_by_external_key(email_prop, &Value::String("alice@example.com".into())),
            Some(eid)
        );
        assert!(
            engine
                .lookup_by_external_key(email_prop, &Value::String("nobody@x.com".into()))
                .is_none()
        );
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lookup_key_survives_flush_and_restart() {
        let dir = temp_dir("lookup_key_persist");
        let email_prop = PropertyId::new(7);
        let eid = EntityId::now_v7();
        {
            let mut engine = Engine::create(&dir).unwrap();
            engine.register_lookup_key(email_prop);
            let mut txn = engine.begin_write();
            txn.put_entity(EntityRecord {
                entity_id: eid,
                type_id: TypeId::new(1),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![(email_prop, Value::String("alice@example.com".into()))],
            });
            txn.commit().unwrap();
            engine.flush().unwrap();
            engine.close().unwrap();
        }
        let mut engine = Engine::open(&dir).unwrap();
        // Must re-register; lookup-key properties live in-memory only in v1.
        engine.register_lookup_key(email_prop);
        // Backfill the registration over already-loaded records.
        engine.rebuild_indexes().unwrap();
        assert_eq!(
            engine.lookup_by_external_key(email_prop, &Value::String("alice@example.com".into())),
            Some(eid)
        );
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn adjacency_finds_hyperedges_per_entity() {
        let dir = temp_dir("adjacency_e2e");
        let mut engine = Engine::create(&dir).unwrap();
        let alice = EntityId::now_v7();
        let bob = EntityId::now_v7();
        let mut hids = Vec::new();
        for _ in 0..5 {
            let h = HyperedgeId::now_v7();
            hids.push(h);
            let mut txn = engine.begin_write();
            txn.put_hyperedge(HyperEdgeRecord {
                hyperedge_id: h,
                type_id: TypeId::new(1),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                roles: vec![(RoleId::new(1), alice), (RoleId::new(2), bob)],
                properties: vec![],
            });
            txn.commit().unwrap();
        }
        let mut alice_hits = engine.hyperedges_for_entity(alice);
        let mut bob_hits = engine.hyperedges_for_entity(bob);
        alice_hits.sort();
        bob_hits.sort();
        hids.sort();
        assert_eq!(alice_hits, hids);
        assert_eq!(bob_hits, hids);
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn property_btree_exact_and_range_after_restart() {
        let dir = temp_dir("propbtree");
        let cust = TypeId::new(1);
        let age = PropertyId::new(10);
        let mut customers: Vec<(EntityId, i64)> = Vec::new();
        {
            let mut engine = Engine::create(&dir).unwrap();
            engine.register_property_btree(cust, age);
            for v in [18_i64, 25, 30, 30, 42, 65, 70] {
                let id = EntityId::now_v7();
                customers.push((id, v));
                let mut txn = engine.begin_write();
                txn.put_entity(EntityRecord {
                    entity_id: id,
                    type_id: cust,
                    tx_id_assert: TxId::new(0),
                    tx_id_supersede: TxId::ACTIVE,
                    properties: vec![(age, Value::I64(v))],
                });
                txn.commit().unwrap();
            }
            // Exact match: two with age=30.
            let at_30 = engine.property_lookup(cust, age, &Value::I64(30));
            assert_eq!(at_30.len(), 2);
            // Range [25, 42].
            let in_range =
                engine.property_range(cust, age, Some(&Value::I64(25)), Some(&Value::I64(42)));
            // 25, 30, 30, 42 = 4 entities.
            assert_eq!(in_range.len(), 4);
            engine.flush().unwrap();
            engine.close().unwrap();
        }
        // Reopen → registrations gone (in-memory in v1). Re-register +
        // backfill via rebuild_indexes.
        let mut engine = Engine::open(&dir).unwrap();
        engine.register_property_btree(cust, age);
        engine.rebuild_indexes().unwrap();
        let in_range =
            engine.property_range(cust, age, Some(&Value::I64(20)), Some(&Value::I64(70)));
        // 25, 30, 30, 42, 65, 70 = 6.
        assert_eq!(in_range.len(), 6);
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vector_search_returns_nearest_after_restart() {
        let dir = temp_dir("vec_search");
        let embedding_prop = PropertyId::new(99);
        let target = EntityId::now_v7();
        {
            let mut engine = Engine::create(&dir).unwrap();
            engine.register_vector_property(embedding_prop);
            let vectors = vec![
                (target, vec![1.0_f32, 0.0, 0.0]),
                (EntityId::now_v7(), vec![0.0, 1.0, 0.0]),
                (EntityId::now_v7(), vec![0.0, 0.0, 1.0]),
                (EntityId::now_v7(), vec![0.9, 0.1, 0.0]),
            ];
            for (id, vec) in vectors {
                let mut txn = engine.begin_write();
                txn.put_entity(EntityRecord {
                    entity_id: id,
                    type_id: TypeId::new(1),
                    tx_id_assert: TxId::new(0),
                    tx_id_supersede: TxId::ACTIVE,
                    properties: vec![(embedding_prop, Value::Vector(vec))],
                });
                txn.commit().unwrap();
            }
            // Pre-restart: confirm search finds target as nearest.
            let hits =
                engine.vector_search(embedding_prop, &[1.0, 0.0, 0.0], 1, Distance::L2Squared);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].0, target);
            engine.flush().unwrap();
            engine.close().unwrap();
        }
        // Reopen: vector index rebuilt; need to re-register the property
        // and call rebuild_indexes to backfill.
        let mut engine = Engine::open(&dir).unwrap();
        engine.register_vector_property(embedding_prop);
        engine.rebuild_indexes().unwrap();
        let hits = engine.vector_search(embedding_prop, &[1.0, 0.0, 0.0], 2, Distance::L2Squared);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, target);
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn type_cluster_groups_hyperedges() {
        let dir = temp_dir("type_cluster_e2e");
        let mut engine = Engine::create(&dir).unwrap();
        for i in 0..6 {
            let mut txn = engine.begin_write();
            txn.put_hyperedge(HyperEdgeRecord {
                hyperedge_id: HyperedgeId::now_v7(),
                type_id: TypeId::new(if i < 4 { 10 } else { 20 }),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                roles: vec![(RoleId::new(1), EntityId::now_v7())],
                properties: vec![],
            });
            txn.commit().unwrap();
        }
        assert_eq!(engine.hyperedges_by_type(TypeId::new(10)).len(), 4);
        assert_eq!(engine.hyperedges_by_type(TypeId::new(20)).len(), 2);
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn adjacency_survives_flush_restart() {
        let dir = temp_dir("adjacency_restart");
        let alice = EntityId::now_v7();
        let mut expected_hids = Vec::new();
        {
            let mut engine = Engine::create(&dir).unwrap();
            for i in 0..10 {
                let h = HyperedgeId::now_v7();
                expected_hids.push(h);
                let mut txn = engine.begin_write();
                txn.put_hyperedge(HyperEdgeRecord {
                    hyperedge_id: h,
                    type_id: TypeId::new(1),
                    tx_id_assert: TxId::new(0),
                    tx_id_supersede: TxId::ACTIVE,
                    roles: vec![(RoleId::new(1), alice)],
                    properties: vec![],
                });
                txn.commit().unwrap();
                if i == 4 {
                    engine.flush().unwrap();
                }
            }
            engine.close().unwrap();
        }
        let engine = Engine::open(&dir).unwrap();
        let mut got = engine.hyperedges_for_entity(alice);
        got.sort();
        expected_hids.sort();
        assert_eq!(got, expected_hids);
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn end_to_end_100_records_with_restart() {
        // Create, write 100 records (50 entities + 50 hyperedges), flush
        // some to SSTable, restart, verify all 100 still readable.
        let dir = temp_dir("e2e_100");
        let mut entities = Vec::new();
        let mut hyperedges = Vec::new();
        {
            let mut engine = Engine::create(&dir).unwrap();
            for i in 0..50 {
                let mut txn = engine.begin_write();
                let eid = EntityId::now_v7();
                entities.push(eid);
                let mut e = make_entity(eid, &format!("entity-{i}"));
                e.type_id = TypeId::new(1 + u32::try_from(i % 3).unwrap());
                txn.put_entity(e);
                txn.commit().unwrap();
                if i == 25 {
                    // Flush halfway through to exercise the SSTable path.
                    engine.flush().unwrap();
                }
            }
            for i in 0..50 {
                let mut txn = engine.begin_write();
                let hid = HyperedgeId::now_v7();
                hyperedges.push(hid);
                let role_entity = entities[i % entities.len()];
                txn.put_hyperedge(HyperEdgeRecord {
                    hyperedge_id: hid,
                    type_id: TypeId::new(5),
                    tx_id_assert: TxId::new(0),
                    tx_id_supersede: TxId::ACTIVE,
                    roles: vec![(RoleId::new(1), role_entity)],
                    properties: vec![],
                });
                txn.commit().unwrap();
            }
            // Don't flush at the end — exercise WAL replay on the second 25
            // entities + all 50 hyperedges.
            engine.close().unwrap();
        }
        let final_tx = {
            let mut engine = Engine::open(&dir).unwrap();
            assert_eq!(engine.sstable_count(), 1, "one mid-loop flush");
            // memtable has the unflushed records.
            assert!(engine.memtable_stats().0 > 0);
            let final_tx = TxId::new(engine.manifest().last_tx_id);
            // Every entity readable.
            for eid in &entities {
                match engine.snapshot_read(&eid.into_uuid(), final_tx).unwrap() {
                    Resolved::Live(Record::Entity(e)) => assert_eq!(&e.entity_id, eid),
                    other => panic!("entity {eid:?} not found: {other:?}"),
                }
            }
            // Every hyperedge readable.
            for hid in &hyperedges {
                match engine.snapshot_read(&hid.into_uuid(), final_tx).unwrap() {
                    Resolved::Live(Record::HyperEdge(h)) => assert_eq!(&h.hyperedge_id, hid),
                    other => panic!("hyperedge {hid:?} not found: {other:?}"),
                }
            }
            engine.close().unwrap();
            final_tx
        };
        // One more cycle for good measure — second close/reopen must still
        // find everything.
        {
            let mut engine = Engine::open(&dir).unwrap();
            assert!(matches!(
                engine
                    .snapshot_read(&entities[7].into_uuid(), final_tx)
                    .unwrap(),
                Resolved::Live(Record::Entity(_))
            ));
            engine.close().unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn snapshot_isolation_old_snapshot_doesnt_see_new_versions() {
        let dir = temp_dir("snapshot_isolation");
        let mut engine = Engine::create(&dir).unwrap();
        let eid = EntityId::now_v7();

        // Tx 1: insert "v1".
        let mut txn = engine.begin_write();
        let mut e = make_entity(eid, "v1");
        e.type_id = TypeId::new(1);
        txn.put_entity(e);
        let snap_v1 = txn.commit().unwrap();

        // Tx 2: insert "v2".
        let mut txn = engine.begin_write();
        let mut e = make_entity(eid, "v2");
        e.type_id = TypeId::new(1);
        txn.put_entity(e);
        let snap_v2 = txn.commit().unwrap();

        // Snapshot at snap_v1: should see "v1".
        match engine.snapshot_read(&eid.into_uuid(), snap_v1).unwrap() {
            Resolved::Live(Record::Entity(e)) => {
                if let Value::String(s) = &e.properties[0].1 {
                    assert_eq!(s, "v1");
                } else {
                    panic!();
                }
            }
            other => panic!("expected v1 at snap_v1, got {other:?}"),
        }
        // Snapshot at snap_v2: should see "v2".
        match engine.snapshot_read(&eid.into_uuid(), snap_v2).unwrap() {
            Resolved::Live(Record::Entity(e)) => {
                if let Value::String(s) = &e.properties[0].1 {
                    assert_eq!(s, "v2");
                } else {
                    panic!();
                }
            }
            other => panic!("expected v2 at snap_v2, got {other:?}"),
        }
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_returns_resolved_deleted() {
        let dir = temp_dir("delete");
        let mut engine = Engine::create(&dir).unwrap();
        let eid = EntityId::now_v7();

        let mut txn = engine.begin_write();
        txn.put_entity(make_entity(eid, "alive"));
        let snap_alive = txn.commit().unwrap();

        let mut txn = engine.begin_write();
        txn.delete(eid.into_uuid());
        let snap_deleted = txn.commit().unwrap();

        // Older snapshot still sees the entity alive.
        assert!(matches!(
            engine.snapshot_read(&eid.into_uuid(), snap_alive).unwrap(),
            Resolved::Live(_)
        ));
        // Newer snapshot sees Deleted.
        match engine
            .snapshot_read(&eid.into_uuid(), snap_deleted)
            .unwrap()
        {
            Resolved::Deleted { deleted_at } => assert_eq!(deleted_at, snap_deleted),
            other => panic!("expected Deleted, got {other:?}"),
        }
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compact_with_floor_preserves_versions_active_readers_might_need() {
        let dir = temp_dir("compact-floor");
        let mut engine = Engine::create(&dir).unwrap();
        let eid = EntityId::now_v7();
        // Commit v1, v2, v3, v4. Each is one tx_id.
        let mut tx_ids = Vec::new();
        for v in 1..=4 {
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(eid, &format!("v{v}")));
            tx_ids.push(txn.commit().unwrap());
            engine.flush().unwrap();
        }
        // Pick the floor = tx_ids[1] (= v2's commit). Versions v1 is
        // shadowed at-or-before floor (next assert = tx_ids[1] which is
        // == floor → next > floor is false → v1 dropped). v2 onward
        // retained because next assert > floor.
        let floor = tx_ids[1];
        let stats = engine.compact_with_floor(floor).unwrap();
        assert!(stats.new_sstable_seq.is_some());

        // Confirm v1 is gone, v2/v3/v4 retained.
        let entities = count_records_of_kind(&dir, crate::record::RecordKind::Entity);
        assert_eq!(entities, 3, "v1 dropped, v2/v3/v4 retained");

        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compact_with_floor_active_matches_default_compact_behaviour() {
        // floor = TxId::ACTIVE should behave identically to compact()
        // (drop everything but the latest version).
        let dir = temp_dir("compact-floor-active");
        let mut engine = Engine::create(&dir).unwrap();
        let eid = EntityId::now_v7();
        for v in 1..=3 {
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(eid, &format!("v{v}")));
            txn.commit().unwrap();
            engine.flush().unwrap();
        }
        engine.compact_with_floor(TxId::ACTIVE).unwrap();
        let entities = count_records_of_kind(&dir, crate::record::RecordKind::Entity);
        assert_eq!(entities, 1, "floor=ACTIVE = aggressive drop = v1.3 baseline");
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn snapshot_iter_streaming_matches_materialised() {
        let dir = temp_dir("stream-match");
        let mut engine = Engine::create(&dir).unwrap();
        // Mix: commits across two flushes so we have memtable + 2 SSTables.
        for batch in 0..2 {
            for i in 0..7 {
                let mut txn = engine.begin_write();
                txn.put_entity(make_entity(EntityId::now_v7(), &format!("b{batch}-{i}")));
                txn.commit().unwrap();
            }
            engine.flush().unwrap();
        }
        for i in 0..3 {
            // Memtable resident.
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(EntityId::now_v7(), &format!("live-{i}")));
            txn.commit().unwrap();
        }
        let snap = TxId::new(engine.manifest().last_tx_id);

        let materialised = engine.snapshot_iter(snap).unwrap();
        let streamed: Vec<_> = engine
            .snapshot_iter_streaming(snap)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            materialised.len(),
            streamed.len(),
            "materialised vs streamed count must match"
        );
        // Both must be sorted by SSTableKey ascending, so element-wise
        // equality is the right check.
        assert_eq!(materialised, streamed);
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn snapshot_iter_streaming_early_termination_stops_pumping() {
        let dir = temp_dir("stream-early");
        let mut engine = Engine::create(&dir).unwrap();
        for _ in 0..50 {
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(EntityId::now_v7(), "x"));
            txn.commit().unwrap();
        }
        let snap = TxId::new(engine.manifest().last_tx_id);
        let first_few: Vec<_> = engine
            .snapshot_iter_streaming(snap)
            .take(5)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(first_few.len(), 5);
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn commit_timestamps_and_retention_persist_across_restart() {
        let dir = temp_dir("persist-meta");
        // Phase 1: set retention + commit some entities, capture the
        // tx + its wall-clock timestamp.
        let (saved_tx, saved_ms);
        {
            let mut engine = Engine::create(&dir).unwrap();
            engine.set_retention_policy(
                TypeId::new(42),
                RetentionPolicy::Versioned { keep_last_n: 7 },
            );
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(EntityId::now_v7(), "first"));
            saved_tx = txn.commit().unwrap();
            saved_ms = engine.commit_timestamp_us(saved_tx).expect("ts recorded");
            engine.flush().unwrap();
            engine.close().unwrap();
        }
        // Phase 2: reopen — retention + timestamps must reload from disk.
        let engine = Engine::open(&dir).unwrap();
        assert_eq!(
            engine.retention_policy(TypeId::new(42)),
            RetentionPolicy::Versioned { keep_last_n: 7 },
            "retention policy survives restart"
        );
        let restored = engine.commit_timestamp_us(saved_tx);
        assert_eq!(restored, Some(saved_ms), "commit timestamp survives restart");
        // tx_at_or_before still works at the same time.
        assert_eq!(engine.tx_at_or_before(saved_ms + 1), Some(saved_tx));
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn serializable_txn_with_no_conflicting_writes_commits() {
        let dir = temp_dir("ssi-happy");
        let mut engine = Engine::create(&dir).unwrap();
        let eid = EntityId::now_v7();
        {
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(eid, "v1"));
            txn.commit().unwrap();
        }
        // Serializable txn that reads `eid`, then writes a new entity.
        // No concurrent writer in v1 — should commit cleanly.
        {
            let mut txn = engine
                .begin_write()
                .with_isolation(IsolationLevel::Serializable);
            let r = txn.read(&eid.into_uuid()).unwrap();
            assert!(matches!(r, Resolved::Live(_)));
            txn.put_entity(make_entity(EntityId::now_v7(), "child"));
            txn.commit().unwrap();
        }
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn serializable_detects_synthetic_read_then_writer_modify() {
        // Construct a synthetic conflict by manually adjusting the
        // tracked read_set's snapshot to a value BEFORE the read key's
        // current tx_id_assert. This exercises the conflict-detection
        // code path even though v1's single-writer model can't naturally
        // produce it.
        let dir = temp_dir("ssi-conflict");
        let mut engine = Engine::create(&dir).unwrap();
        let eid = EntityId::now_v7();
        let first_tx = {
            let mut txn = engine.begin_write();
            let tx = txn.tx_id();
            txn.put_entity(make_entity(eid, "v1"));
            txn.commit().unwrap();
            tx
        };
        let second_tx = {
            let mut txn = engine.begin_write();
            let tx = txn.tx_id();
            txn.put_entity(make_entity(eid, "v2"));
            txn.commit().unwrap();
            tx
        };
        assert!(second_tx > first_tx);

        // Now open a Serializable txn and inject a "stale" read at
        // first_tx (the pre-modification snapshot). The commit-time
        // check will see the entity has been modified at second_tx
        // since first_tx and abort.
        let mut txn = engine
            .begin_write()
            .with_isolation(IsolationLevel::Serializable);
        // Direct injection — emulates what a multi-writer engine would
        // do via Self::read at a prior snapshot.
        txn.read_set.push((eid.into_uuid(), first_tx));
        txn.put_entity(make_entity(EntityId::now_v7(), "derived"));
        let err = txn.commit().unwrap_err();
        assert!(matches!(err, EngineError::SerializationFailure { .. }));
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn metadata_constraints_load_at_open() {
        use crate::validation::{
            CONSTRAINT_KIND_REQUIRED, CONSTRAINT_KIND_VALUE_TAG, PROP_CONSTRAINT_KIND,
            PROP_EXPECTED_TAG, PROP_TARGET_PROPERTY, PROP_TARGET_TYPE, TYPE_VALIDATION_CONSTRAINT,
        };
        use crate::value::TAG_STRING;

        let dir = temp_dir("meta-constraints");
        // Phase 1: commit two constraint entities + close.
        {
            let mut engine = Engine::create(&dir).unwrap();
            // Required: type 50, property 60.
            let mut txn = engine.begin_write();
            txn.put_entity(EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: TYPE_VALIDATION_CONSTRAINT,
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![
                    (PROP_CONSTRAINT_KIND, Value::I64(CONSTRAINT_KIND_REQUIRED)),
                    (PROP_TARGET_TYPE, Value::I64(50)),
                    (PROP_TARGET_PROPERTY, Value::I64(60)),
                ],
            });
            txn.commit().unwrap();
            // Value tag: type 50, property 60 must be String (tag 0x05).
            let mut txn = engine.begin_write();
            txn.put_entity(EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: TYPE_VALIDATION_CONSTRAINT,
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![
                    (PROP_CONSTRAINT_KIND, Value::I64(CONSTRAINT_KIND_VALUE_TAG)),
                    (PROP_TARGET_TYPE, Value::I64(50)),
                    (PROP_TARGET_PROPERTY, Value::I64(60)),
                    (PROP_EXPECTED_TAG, Value::I64(i64::from(TAG_STRING))),
                ],
            });
            txn.commit().unwrap();
            engine.close().unwrap();
        }

        // Phase 2: reopen — constraints should be loaded automatically.
        let mut engine = Engine::open(&dir).unwrap();
        assert!(engine.validation().has_constraints());
        // Try committing an entity that violates the required-property rule.
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(50),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![],
        });
        let err = txn.commit().unwrap_err();
        assert!(matches!(
            err,
            EngineError::Validation(ValidationError::MissingRequiredProperty { .. })
        ));
        // Now commit with the property present BUT wrong tag — value-tag
        // constraint should fire.
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(50),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(60), Value::I64(99))],
        });
        let err = txn.commit().unwrap_err();
        assert!(matches!(
            err,
            EngineError::Validation(ValidationError::WrongValueTag { .. })
        ));
        // Correct shape commits cleanly.
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(50),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(60), Value::String("ok".into()))],
        });
        txn.commit().unwrap();
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn retention_policy_audited_preserves_every_version() {
        let dir = temp_dir("retention-audit");
        let mut engine = Engine::create(&dir).unwrap();
        let type_id = TypeId::new(7);
        engine.set_retention_policy(type_id, RetentionPolicy::Audited);
        let eid = EntityId::now_v7();

        // Three versions of the same entity, three commits + flushes so
        // they land in distinct SSTables.
        for v in 1..=3 {
            let mut txn = engine.begin_write();
            txn.put_entity(EntityRecord {
                entity_id: eid,
                type_id,
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![(PropertyId::new(1), Value::I64(v))],
            });
            txn.commit().unwrap();
            engine.flush().unwrap();
        }
        let stats = engine.compact().unwrap();
        assert_eq!(stats.sstables_in, 3);
        // v2.0: each commit also writes a durable TxTimestamp record;
        // set_retention_policy writes a RetentionPolicy + TxTimestamp.
        // After Audited compaction of the entity type:
        //   3 entity versions (Audited)
        // + 4 TxTimestamp groups (1 per commit incl. set_retention) — each LatestOnly
        // + 1 RetentionPolicy group — LatestOnly
        // = 8 records out. Entity count alone is the meaningful assert.
        let entities_out = count_records_of_kind(&dir, crate::record::RecordKind::Entity);
        assert_eq!(entities_out, 3, "Audited must preserve every entity version");

        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Helper used by retention tests — count how many records of a given
    /// kind exist in the engine's SSTables. Opens fresh readers to avoid
    /// disturbing engine state.
    fn count_records_of_kind(dir: &std::path::Path, kind: crate::record::RecordKind) -> usize {
        let mut n = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_some_and(|e| e == "ndb") {
                let r = SSTableReader::open(&p).unwrap();
                for item in r.iter() {
                    let (rec, _) = item.unwrap();
                    if rec.kind() == kind {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    #[test]
    fn retention_policy_versioned_keeps_last_n() {
        let dir = temp_dir("retention-versioned");
        let mut engine = Engine::create(&dir).unwrap();
        let type_id = TypeId::new(8);
        engine.set_retention_policy(type_id, RetentionPolicy::Versioned { keep_last_n: 2 });
        let eid = EntityId::now_v7();

        // Five versions across five SSTables.
        for v in 1..=5 {
            let mut txn = engine.begin_write();
            txn.put_entity(EntityRecord {
                entity_id: eid,
                type_id,
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![(PropertyId::new(1), Value::I64(v))],
            });
            txn.commit().unwrap();
            engine.flush().unwrap();
        }
        let stats = engine.compact().unwrap();
        assert_eq!(stats.sstables_in, 5);
        // Entity-only count: Versioned { keep_last_n: 2 } keeps 2.
        let entities_out = count_records_of_kind(&dir, crate::record::RecordKind::Entity);
        assert_eq!(entities_out, 2, "Versioned keep_last_n=2");

        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn retention_policy_default_latest_only_unchanged() {
        let dir = temp_dir("retention-default");
        let mut engine = Engine::create(&dir).unwrap();
        let eid = EntityId::now_v7();
        for v in 1..=3 {
            let mut txn = engine.begin_write();
            txn.put_entity(EntityRecord {
                entity_id: eid,
                type_id: TypeId::new(9),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![(PropertyId::new(1), Value::I64(v))],
            });
            txn.commit().unwrap();
            engine.flush().unwrap();
        }
        let stats = engine.compact().unwrap();
        assert_eq!(stats.sstables_in, 3);
        // Entity-only count: no policy → LatestOnly → 1 surviving entity.
        let entities_out = count_records_of_kind(&dir, crate::record::RecordKind::Entity);
        assert_eq!(entities_out, 1, "LatestOnly default");
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rollback_discards_pending_writes() {
        let dir = temp_dir("rollback");
        let mut engine = Engine::create(&dir).unwrap();
        let eid = EntityId::now_v7();
        let tx_id;
        {
            let mut txn = engine.begin_write();
            tx_id = txn.tx_id();
            txn.put_entity(make_entity(eid, "ghost"));
            txn.rollback();
        }
        // The entity must NOT be visible at any snapshot.
        assert!(matches!(
            engine.snapshot_read(&eid.into_uuid(), tx_id).unwrap(),
            Resolved::Missing
        ));
        // tx_id was allocated, so the next commit gets a later one.
        let mut txn = engine.begin_write();
        assert!(txn.tx_id() > tx_id);
        txn.put_entity(make_entity(EntityId::now_v7(), "real"));
        txn.commit().unwrap();
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
