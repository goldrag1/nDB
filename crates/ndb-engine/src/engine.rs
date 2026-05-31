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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::db::{Database, DatabaseError, Manifest, ManifestEntry};
use crate::encryption::{
    Cipher, DEFAULT_CHUNK_SIZE, ENCRYPTION_MARKER_FILENAME, ENCRYPTION_MIGRATION_FILENAME,
    EncryptionError, EncryptionMarker,
};
use crate::error::EncodeError;
use crate::id::{EntityId, HyperedgeId, PropertyId, TX_ACTIVE, TxId, TypeId};
use crate::index::property_btree::value_to_index_bytes;
use crate::index::lookup_key::value_to_index_bytes as lookup_value_to_index_bytes;
use crate::index::vector::distance as vector_distance;
use crate::index::property_index_file::{
    PropertyIndexBuilder, PropertyIndexFile, sidecar_path_for as pidx_sidecar_path_for,
};
use crate::index::vector_index_file::{
    VectorIndexBuilder, VectorIndexFile, sidecar_path_for as vidx_sidecar_path_for,
    write_streaming_single as write_vsnap_streaming,
};
use crate::index::id_list_index_file::{
    IdListIndexBuilder, IdListIndexFile, sidecar_path_for as idl_sidecar_path_for,
};
use crate::index::{
    AdjacencyIndex, Distance, EntityTypeIndex, HyperEdgeTypeIndex, Index, LookupKeyIndex,
    PropertyBTreeIndex, VectorIndex,
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

// id-list index sidecar tags + extensions (low-RAM core, Phase 2e).
const ADJ_MAGIC: [u8; 4] = *b"NADJ";
const ADJ_EXT: &str = "adjx";
const TYC_MAGIC: [u8; 4] = *b"NTYC";
const TYC_EXT: &str = "tycx";
const ETC_MAGIC: [u8; 4] = *b"NETC";
const ETC_EXT: &str = "etcx";
const LKP_MAGIC: [u8; 4] = *b"NLKP";
const LKP_EXT: &str = "lkpx";
// Metadata sidecar: TxTimestamp + RetentionPolicy records, so a low-RAM
// open can populate commit_timestamps + retention WITHOUT scanning every
// SSTable (the needs_scan skip in rebuild_indexes).
const META_MAGIC: &[u8; 4] = b"NDMT";
const META_EXT: &str = "meta";

/// Encode the per-SSTable metadata sidecar.
fn encode_meta(ts: &[(u64, i64)], ret: &[(u32, u8, u32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + ts.len() * 16 + ret.len() * 9 + 4);
    out.extend_from_slice(META_MAGIC);
    out.push(1); // version
    out.extend_from_slice(&[0u8; 3]);
    out.extend_from_slice(&u32::try_from(ts.len()).unwrap_or(u32::MAX).to_le_bytes());
    for (tx, t) in ts {
        out.extend_from_slice(&tx.to_le_bytes());
        out.extend_from_slice(&t.to_le_bytes());
    }
    out.extend_from_slice(&u32::try_from(ret.len()).unwrap_or(u32::MAX).to_le_bytes());
    for (ty, kind, n) in ret {
        out.extend_from_slice(&ty.to_le_bytes());
        out.push(*kind);
        out.extend_from_slice(&n.to_le_bytes());
    }
    let mut h = crc32fast::Hasher::new();
    h.update(&out);
    out.extend_from_slice(&h.finalize().to_le_bytes());
    out
}

/// `(tx_id, timestamp_us)` rows in a `.meta` sidecar.
type MetaTimestamps = Vec<(u64, i64)>;
/// `(type_id, policy_kind, keep_last_n)` rows in a `.meta` sidecar.
type MetaRetention = Vec<(u32, u8, u32)>;

/// Decode a metadata sidecar → (tx timestamps, retention rows). Returns
/// `None` on any corruption (caller falls back to a full rebuild scan).
fn decode_meta(bytes: &[u8]) -> Option<(MetaTimestamps, MetaRetention)> {
    if bytes.len() < 16 || &bytes[0..4] != META_MAGIC || bytes[4] != 1 {
        return None;
    }
    let trailer = bytes.len().checked_sub(4)?;
    let stored = u32::from_le_bytes(bytes[trailer..].try_into().ok()?);
    let mut h = crc32fast::Hasher::new();
    h.update(&bytes[..trailer]);
    if stored != h.finalize() {
        return None;
    }
    let mut pos = 8;
    let ts_count = u32::from_le_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;
    let mut ts = Vec::with_capacity(ts_count);
    for _ in 0..ts_count {
        let tx = u64::from_le_bytes(bytes.get(pos..pos + 8)?.try_into().ok()?);
        let t = i64::from_le_bytes(bytes.get(pos + 8..pos + 16)?.try_into().ok()?);
        ts.push((tx, t));
        pos += 16;
    }
    let ret_count = u32::from_le_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;
    let mut ret = Vec::with_capacity(ret_count);
    for _ in 0..ret_count {
        let ty = u32::from_le_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?);
        let kind = *bytes.get(pos + 4)?;
        let n = u32::from_le_bytes(bytes.get(pos + 5..pos + 9)?.try_into().ok()?);
        ret.push((ty, kind, n));
        pos += 9;
    }
    Some((ts, ret))
}

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

/// Statistics returned by [`Engine::backup_to`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackupStats {
    /// Total files copied into the backup directory (data + sidecars +
    /// manifests + CURRENT + encryption marker).
    pub files_copied: u64,
    /// Total bytes copied.
    pub bytes_copied: u64,
    /// Number of SSTable (`.ndb`) data files copied.
    pub sstables: u64,
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

    /// At-rest encryption is misconfigured: the running key does not
    /// match the on-disk `.encryption` marker, or one side has a key
    /// where the other does not.
    ///
    /// Specifically:
    ///   - Env `NDB_ENC_KEY` set, marker present, fingerprints differ.
    ///   - Env unset, marker present (database was encrypted; opening
    ///     plaintext would silently corrupt it).
    ///   - Env set, marker absent (database is plaintext; opening
    ///     encrypted would silently re-encrypt). Use
    ///     `Engine::reencrypt` (v2.1) to migrate explicitly.
    #[error("encryption_key_mismatch: {detail}")]
    EncryptionKeyMismatch {
        /// Human-readable explanation, including which side carries
        /// which state.
        detail: String,
    },

    /// A prior `Engine::reencrypt` call did not run to completion —
    /// the transient `.encryption.next` marker is still on disk. The
    /// database may have a mix of files encrypted under the old vs
    /// new keys; refusing to open is the safe default. Manual recovery
    /// requires supplying both keys; the simple path is to restore
    /// from backup.
    #[error("encryption_migration_incomplete: {detail}")]
    EncryptionMigrationIncomplete {
        /// Human-readable detail.
        detail: String,
    },

    /// At-rest encryption primitive failed (marker decode, AEAD failure,
    /// invalid key length, hex decode).
    #[error(transparent)]
    Encryption(#[from] EncryptionError),
}

/// Stats returned by [`Engine::reencrypt`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MigrationStats {
    /// Number of SSTable files rewritten under the new cipher.
    pub sstables_rewritten: usize,
    /// Number of WAL segments rewritten. Always 0 or 1 in v2.1 (single
    /// active WAL).
    pub wal_segments_rewritten: usize,
    /// Total bytes of original file content that were rewritten.
    /// Approximates I/O cost; not exact (post-rewrite size may differ
    /// because per-chunk AEAD overhead differs between cipher states).
    pub bytes_rewritten: u64,
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
// Engine configuration (low-RAM core, Option B — see
// docs/specs/2026-05-29-low-ram-core-option-b.md)
// ---------------------------------------------------------------------------

/// Default block-cache budget: 2 GiB.
pub const DEFAULT_MAX_CACHE_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Tunables controlling resident-memory behaviour. Built so that the
/// `Default` value reproduces the historical engine behaviour exactly —
/// `Engine::open` is `open_with_config(path, EngineConfig::default())`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// Hard ceiling for the Phase-3 bounded block cache, in bytes.
    /// Inert until the block cache lands; carried now so the surface is
    /// stable. Default [`DEFAULT_MAX_CACHE_BYTES`] (2 GiB).
    pub max_cache_bytes: usize,
    /// Serve on-disk-capable secondary indexes from mmap'd sidecars
    /// instead of rebuilding them in RAM on `open`. Default `false`
    /// (historical behaviour). `low_memory` forces this on.
    pub mmap_indexes: bool,
    /// Convenience preset for memory-constrained deployments. Implies
    /// `mmap_indexes` and a tighter cache budget via [`Self::resolved`].
    pub low_memory: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_cache_bytes: DEFAULT_MAX_CACHE_BYTES,
            mmap_indexes: false,
            low_memory: false,
        }
    }
}

impl EngineConfig {
    /// A config tuned for low-RAM operation: mmap'd indexes + the given
    /// cache budget.
    #[must_use]
    pub fn low_memory(max_cache_bytes: usize) -> Self {
        Self {
            max_cache_bytes,
            mmap_indexes: true,
            low_memory: true,
        }
    }

    /// Normalise interdependent fields: `low_memory` implies
    /// `mmap_indexes`. Returns the effective config the engine acts on.
    #[must_use]
    pub fn resolved(self) -> Self {
        Self {
            mmap_indexes: self.mmap_indexes || self.low_memory,
            ..self
        }
    }
}

/// Per-index resident heap estimate (bytes), returned by
/// [`Engine::index_memory_stats`]. Drives the RAM-vs-DB-size baseline
/// curve the low-RAM work is reducing. Estimates capture dominant terms
/// (key/value bytes + per-entry container overhead), not exact allocator
/// footprint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexMemoryStats {
    /// Lookup-key reverse index.
    pub lookup_key: usize,
    /// Adjacency (entity → hyperedges).
    pub adjacency: usize,
    /// Hyperedge-type clustering.
    pub type_cluster: usize,
    /// Entity-type clustering.
    pub entity_type_cluster: usize,
    /// Brute-force vector index (embeddings dominate).
    pub vector: usize,
    /// Property B-tree.
    pub property_btree: usize,
    /// Memtable size estimate (already on-disk-bounded by flush threshold).
    pub memtable: usize,
}

impl IndexMemoryStats {
    /// Total secondary-index resident estimate (excludes memtable).
    #[must_use]
    pub fn index_total(&self) -> usize {
        self.lookup_key
            + self.adjacency
            + self.type_cluster
            + self.entity_type_cluster
            + self.vector
            + self.property_btree
    }

    /// Grand total including the memtable estimate.
    #[must_use]
    pub fn total(&self) -> usize {
        self.index_total() + self.memtable
    }
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
    /// At-rest cipher loaded when `NDB_ENC_KEY` is set and the on-disk
    /// `.encryption` marker confirms a fingerprint match. `None` ⇒ the
    /// database is plaintext. Every WAL append + SSTable write/read
    /// path consults this; encrypted databases refuse to open without
    /// a key, plaintext databases refuse to open with one.
    cipher: Option<Cipher>,
    /// Lookup-key reverse index — `(property_id, value) → entity_id`.
    lookup_key: LookupKeyIndex,
    /// Adjacency index — `entity → [hyperedges referencing it]`.
    adjacency: AdjacencyIndex,
    /// Hyperedge-type clustering — `type_id → [hyperedge ids]`.
    type_cluster: HyperEdgeTypeIndex,
    /// Entity-type clustering — `type_id → [entity ids]`. Load-bearing
    /// for the v3 count-aggregate fast path.
    entity_type_cluster: EntityTypeIndex,
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
    /// Resident-memory tunables. `EngineConfig::default()` reproduces the
    /// historical behaviour; opt into low-RAM mode via `open_with_config`.
    config: EngineConfig,
    /// mmap'd on-disk property index sidecars, keyed by their SSTable path.
    /// Populated only under `config.mmap_indexes`. SSTables present here are
    /// served from disk; SSTables absent here (no sidecar) fall back to the
    /// in-RAM `property_btree` mirror, which under mmap mode holds only the
    /// memtable + sidecar-less data.
    property_index_files: HashMap<PathBuf, PropertyIndexFile>,
    /// mmap'd on-disk vector index sidecars, keyed by SSTable path. Same
    /// lifecycle + fallback as `property_index_files`. Embeddings are the
    /// dominant resident-RAM term, so this is the main low-RAM win.
    vector_index_files: HashMap<PathBuf, VectorIndexFile>,
    /// Optional global current-vector SNAPSHOT per property: every SSTable
    /// flush writes its own `<seq>.vidx`, so `vector_search` fans out across
    /// hundreds of sidecars + MVCC-verifies each candidate (the O(sidecars×k)
    /// random-read wall — ~15 s at 10 GB). A snapshot collapses all CURRENT
    /// vectors into ONE mmap'd `.vsnap` (same format), searched directly with
    /// NO fan-out and NO per-candidate verify. Built once + persisted (like
    /// the app's top cache); valid for read-mostly serving — rebuild after
    /// writes. property_id → reader.
    vector_snapshots: HashMap<u32, VectorIndexFile>,
    /// mmap'd id-list sidecars for the four remaining secondary indexes
    /// (entity→hyperedges, type→hyperedges, type→entities, key→entity).
    /// Same lifecycle + fallback as the others; populated only under
    /// `config.mmap_indexes`.
    adjacency_files: HashMap<PathBuf, IdListIndexFile>,
    type_cluster_files: HashMap<PathBuf, IdListIndexFile>,
    entity_type_files: HashMap<PathBuf, IdListIndexFile>,
    lookup_key_files: HashMap<PathBuf, IdListIndexFile>,
}

impl Engine {
    /// Create a fresh plaintext database directory and engine.
    ///
    /// For an encrypted database, use [`Engine::create_with_cipher`].
    /// The bare `create()` deliberately does NOT consult `NDB_ENC_KEY`
    /// — server binaries that want env-driven encryption call
    /// `create_from_env` (or build the cipher themselves and pass it
    /// to `create_with_cipher`). Library callers stay in control.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, EngineError> {
        Self::create_with_cipher(path, None)
    }

    /// Create with an explicit [`EngineConfig`]. Low-RAM core: the config
    /// must be set at create time (not just open) for the engine to write
    /// index sidecars during ingest. `EngineConfig::default()` reproduces
    /// the historical create.
    pub fn create_with_config<P: AsRef<Path>>(
        path: P,
        config: EngineConfig,
    ) -> Result<Self, EngineError> {
        Self::create_with_cipher_config(path, None, config)
    }

    /// Create a fresh database with an explicit at-rest cipher. When
    /// `cipher = Some(_)`, the `.encryption` marker is written before
    /// the first WAL allocation and every subsequent WAL append +
    /// SSTable write goes through AES-GCM-256.
    pub fn create_with_cipher<P: AsRef<Path>>(
        path: P,
        cipher: Option<Cipher>,
    ) -> Result<Self, EngineError> {
        Self::create_with_cipher_config(path, cipher, EngineConfig::default())
    }

    /// Create with both an explicit cipher and an [`EngineConfig`]. All
    /// other `create*` entry points funnel here.
    pub fn create_with_cipher_config<P: AsRef<Path>>(
        path: P,
        cipher: Option<Cipher>,
        config: EngineConfig,
    ) -> Result<Self, EngineError> {
        let config = config.resolved();
        let mut db = Database::create(path)?;
        if let Some(c) = cipher.as_ref() {
            let marker = EncryptionMarker::new(c, DEFAULT_CHUNK_SIZE);
            write_encryption_marker(db.path(), &marker)?;
        }
        let wal_seq = db.allocate_file_seq();
        let wal_path = wal_path(db.path(), wal_seq);
        let wal = WriteAheadLog::create_with_cipher(&wal_path, cipher.clone())?;
        let mut manifest = db.manifest().clone();
        manifest.active_wal_seq = wal_seq;
        db.write_manifest(manifest)?;
        Ok(Self {
            memtable: Memtable::new(),
            sstables: Vec::new(),
            wal: Some(wal),
            cipher,
            lookup_key: LookupKeyIndex::new(),
            adjacency: AdjacencyIndex::new(),
            type_cluster: HyperEdgeTypeIndex::new(),
            entity_type_cluster: EntityTypeIndex::new(),
            vector: VectorIndex::new(),
            property_btree: PropertyBTreeIndex::new(),
            validation: ValidationEngine::new(),
            db,
            commit_timestamps: std::collections::BTreeMap::new(),
            retention: HashMap::new(),
            config,
            property_index_files: HashMap::new(),
            vector_index_files: HashMap::new(),
            vector_snapshots: HashMap::new(),
            adjacency_files: HashMap::new(),
            type_cluster_files: HashMap::new(),
            entity_type_files: HashMap::new(),
            lookup_key_files: HashMap::new(),
        })
    }

    /// Create a fresh database. Cipher is sourced from `NDB_ENC_KEY` —
    /// if set, the database starts life encrypted; if unset, plaintext.
    ///
    /// This is the entry point intended for `ndb-server` and `ndb-cli`;
    /// library code should prefer the explicit `create_with_cipher` so
    /// tests don't accidentally race against the env.
    pub fn create_from_env<P: AsRef<Path>>(path: P) -> Result<Self, EngineError> {
        let cipher = Cipher::from_env()?;
        Self::create_with_cipher(path, cipher)
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
        Self::open_with_cipher(path, None)
    }

    /// Open with an explicit [`EngineConfig`] (low-RAM core opt-in).
    /// `open_with_config(path, EngineConfig::default())` is identical to
    /// `open(path)`.
    pub fn open_with_config<P: AsRef<Path>>(
        path: P,
        config: EngineConfig,
    ) -> Result<Self, EngineError> {
        Self::open_with_cipher_config(path, None, config)
    }

    /// Open an existing database with an explicit cipher hint. The
    /// hint is reconciled against the on-disk `.encryption` marker via
    /// [`resolve_cipher_against_marker`]; mismatches raise
    /// [`EngineError::EncryptionKeyMismatch`].
    pub fn open_with_cipher<P: AsRef<Path>>(
        path: P,
        hint: Option<Cipher>,
    ) -> Result<Self, EngineError> {
        Self::open_with_cipher_config(path, hint, EngineConfig::default())
    }

    /// Open with both an explicit cipher hint and an [`EngineConfig`].
    /// All other `open*` entry points funnel here.
    #[allow(clippy::too_many_lines)]
    pub fn open_with_cipher_config<P: AsRef<Path>>(
        path: P,
        hint: Option<Cipher>,
        config: EngineConfig,
    ) -> Result<Self, EngineError> {
        let config = config.resolved();
        let mut db = Database::open(path)?;

        // Reconcile the supplied cipher (if any) against the marker on
        // disk. Refuses encrypted-no-key, plaintext-with-key, and
        // wrong-key opens.
        let cipher = resolve_cipher_against_marker(db.path(), hint)?;

        // Open active SSTables.
        let mut sstables: Vec<SSTableReader> = Vec::new();
        // Sort entries by level then file_seq descending so newest layer is
        // first in the lookup chain.
        let mut entries = db.manifest().sstables.clone();
        entries.sort_by(|a, b| a.level.cmp(&b.level).then(b.file_seq.cmp(&a.file_seq)));
        for entry in &entries {
            let p = sstable_path(db.path(), entry.file_seq);
            sstables.push(SSTableReader::open_with_cipher(&p, cipher.clone())?);
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
            WriteAheadLog::create_with_cipher(&p, cipher.clone())?
        } else {
            let p = wal_path(db.path(), wal_seq);
            let (safe_end, max_tx_seen) = replay_wal_into(&p, &mut memtable, cipher.clone())?;
            if cipher.is_none() {
                // Plaintext WAL: truncate at the safe boundary so the
                // next append starts at a clean spot.
                truncate_to(&p, safe_end)?;
            }
            if max_tx_seen > db.manifest().last_tx_id {
                // Reconcile the MANIFEST with what the WAL just told us
                // happened since the last flush. Persist immediately so a
                // subsequent crash before the next flush doesn't re-stale
                // the watermark.
                let mut m = db.manifest().clone();
                m.last_tx_id = max_tx_seen;
                db.write_manifest(m)?;
            }
            if cipher.is_some() {
                // Encrypted WALs can't be mid-file appended — rotate
                // every open. The just-replayed records are already in
                // the memtable and will be re-WAL'd on the next commit.
                let new_seq = db.allocate_file_seq();
                let new_p = wal_path(db.path(), new_seq);
                let new_wal = WriteAheadLog::create_with_cipher(&new_p, cipher.clone())?;
                let mut m = db.manifest().clone();
                m.active_wal_seq = new_seq;
                db.write_manifest(m)?;
                // Old segment can be deleted now — every record was replayed.
                let _ = std::fs::remove_file(&p);
                new_wal
            } else {
                WriteAheadLog::open_append_with_cipher(&p, cipher.as_ref())?
            }
        };

        let mut engine = Self {
            memtable,
            sstables,
            wal: Some(wal),
            cipher,
            lookup_key: LookupKeyIndex::new(),
            adjacency: AdjacencyIndex::new(),
            type_cluster: HyperEdgeTypeIndex::new(),
            entity_type_cluster: EntityTypeIndex::new(),
            vector: VectorIndex::new(),
            property_btree: PropertyBTreeIndex::new(),
            validation: ValidationEngine::new(),
            db,
            commit_timestamps: std::collections::BTreeMap::new(),
            retention: HashMap::new(),
            config,
            property_index_files: HashMap::new(),
            vector_index_files: HashMap::new(),
            vector_snapshots: HashMap::new(),
            adjacency_files: HashMap::new(),
            type_cluster_files: HashMap::new(),
            entity_type_files: HashMap::new(),
            lookup_key_files: HashMap::new(),
        };
        // Low-RAM core: open the on-disk index sidecars (under
        // `mmap_indexes`) BEFORE rebuilding RAM indexes, so the rebuild can
        // skip the data already served from disk.
        engine.load_property_index_sidecars();
        engine.load_vector_index_sidecars();
        engine.load_id_list_sidecars();
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

    /// Open an existing database, sourcing the cipher hint from
    /// `NDB_ENC_KEY`. Server / CLI entry point — library code should
    /// prefer `open_with_cipher` so tests don't race on the env.
    pub fn open_from_env<P: AsRef<Path>>(path: P) -> Result<Self, EngineError> {
        let cipher = Cipher::from_env()?;
        Self::open_with_cipher(path, cipher)
    }

    /// Scan the latest snapshot for metadata constraint entities and
    /// register them with the validation engine. Returns the number of
    /// constraints loaded. Called automatically by `open()`; callers
    /// that add constraint entities at runtime can invoke this manually
    /// to pick up the changes.
    pub fn reload_constraints_from_metadata(&mut self) -> Result<usize, EngineError> {
        let snap = TxId::new(self.db.manifest().last_tx_id);
        // Low-RAM: find the (rare) constraint entities by type via the
        // on-disk entity-type index instead of materialising every record —
        // otherwise `open` would scan + materialise the whole DB.
        if self.entity_type_served_from_disk() {
            let ids = self.entities_by_type(crate::validation::TYPE_VALIDATION_CONSTRAINT);
            let mut recs = Vec::with_capacity(ids.len());
            for id in ids {
                if let Ok(Resolved::Live(rec)) = self.snapshot_read(&id.into_uuid(), snap) {
                    recs.push(rec);
                }
            }
            return Ok(self.validation.load_from_metadata(&recs));
        }
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
        self.entity_type_cluster.clear();
        self.vector.clear();
        self.property_btree.clear();
        // Metadata maps (v2.0+) — rebuilt from the durable records.
        self.commit_timestamps.clear();
        self.retention.clear();
        // SSTables (sstables[0] is newest layer; iterate in declared order).
        for sst in &mut self.sstables {
            // Under mmap mode, SSTables with a sidecar serve that index from
            // disk — don't duplicate it into the RAM mirror.
            let mm = self.config.mmap_indexes;
            let property_on_disk = mm && self.property_index_files.contains_key(sst.path());
            let vector_on_disk = mm && self.vector_index_files.contains_key(sst.path());
            let adj_on_disk = mm && self.adjacency_files.contains_key(sst.path());
            let tyc_on_disk = mm && self.type_cluster_files.contains_key(sst.path());
            let etc_on_disk = mm && self.entity_type_files.contains_key(sst.path());
            let lkp_on_disk = mm && self.lookup_key_files.contains_key(sst.path());
            // If every index that could have data here is on disk, there's
            // nothing to apply in RAM — skip the scan entirely. This is what
            // keeps `open` O(1) (no full-DB read, no RSS spike) at scale.
            let needs_scan = !adj_on_disk
                || !tyc_on_disk
                || !etc_on_disk
                || (self.lookup_key.has_registrations() && !lkp_on_disk)
                || (self.vector.has_registrations() && !vector_on_disk)
                || (self.property_btree.has_registrations() && !property_on_disk);
            if !needs_scan {
                continue;
            }
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
                if !lkp_on_disk {
                    self.lookup_key.apply(&rec, tx);
                }
                if !adj_on_disk {
                    self.adjacency.apply(&rec, tx);
                }
                if !tyc_on_disk {
                    self.type_cluster.apply(&rec, tx);
                }
                if !etc_on_disk {
                    self.entity_type_cluster.apply(&rec, tx);
                }
                if !vector_on_disk {
                    self.vector.apply(&rec, tx);
                }
                if !property_on_disk {
                    self.property_btree.apply(&rec, tx);
                }
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
            self.entity_type_cluster.apply(rec, tx);
            self.vector.apply(rec, tx);
            self.property_btree.apply(rec, tx);
        }
        // Low-RAM mode: SSTables skipped above (all indexes on disk) still
        // contributed tx timestamps + retention — load those from the
        // `.meta` sidecars so as-of-timestamp + retention stay correct.
        self.load_meta_sidecars();
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
        if !self.lookup_served_from_disk() {
            return self.lookup_key.lookup(property_id, value);
        }
        let Some(vb) = lookup_value_to_index_bytes(value) else {
            return None;
        };
        let mut key = property_id.get().to_be_bytes().to_vec();
        key.extend_from_slice(&vb);
        let mut cand: HashSet<EntityId> = HashSet::new();
        for f in self.lookup_key_files.values() {
            cand.extend(f.find(&key).into_iter().map(EntityId::from_bytes));
        }
        if let Some(e) = self.lookup_key.lookup(property_id, value) {
            cand.insert(e);
        }
        // Verify (live + current value matches); deterministic smallest id.
        cand.into_iter()
            .filter(|e| self.entity_lookup_matches(*e, property_id, &vb))
            .min()
    }

    /// All hyperedges that reference `entity` in any role.
    #[must_use]
    pub fn hyperedges_for_entity(&self, entity: EntityId) -> Vec<HyperedgeId> {
        if !self.adjacency_served_from_disk() {
            return self.adjacency.neighbors_vec(entity);
        }
        let mut cand: HashSet<HyperedgeId> = HashSet::new();
        for f in self.adjacency_files.values() {
            cand.extend(f.find(entity.as_bytes()).into_iter().map(HyperedgeId::from_bytes));
        }
        cand.extend(self.adjacency.neighbors_vec(entity));
        // Verify: hyperedge live AND still references `entity`.
        let mut out: Vec<HyperedgeId> = cand
            .into_iter()
            .filter(|h| {
                self.current_hyperedge(*h)
                    .is_some_and(|hr| hr.roles.iter().any(|(_, e)| *e == entity))
            })
            .collect();
        out.sort();
        out
    }

    /// Bounded variant of [`hyperedges_for_entity`]: stop after verifying at
    /// most `cap` live incident hyperedges. On a power-law hub (degree ~10^5)
    /// the unbounded method does ~10^5 point reads (`current_hyperedge` per
    /// candidate) — seconds per node. A viz tile only needs a sparse sample,
    /// so this caps both the candidate gather AND the per-candidate verify at
    /// `cap`, bounding disk reads regardless of degree. Order is arbitrary
    /// (whatever the sidecars yield first); fine for sampling, not for a
    /// complete neighbourhood.
    #[must_use]
    pub fn hyperedges_for_entity_capped(&self, entity: EntityId, cap: usize) -> Vec<HyperedgeId> {
        if cap == 0 {
            return Vec::new();
        }
        if !self.adjacency_served_from_disk() {
            let mut v = self.adjacency.neighbors_vec(entity);
            v.truncate(cap);
            return v;
        }
        // Gather candidates, but stop early once we have plenty to verify
        // down to `cap` (gather a small multiple to tolerate dead/superseded
        // edges without re-scanning).
        let gather_cap = cap.saturating_mul(4).max(cap);
        let mut cand: Vec<HyperedgeId> = Vec::new();
        'gather: for f in self.adjacency_files.values() {
            for h in f.find(entity.as_bytes()) {
                cand.push(HyperedgeId::from_bytes(h));
                if cand.len() >= gather_cap {
                    break 'gather;
                }
            }
        }
        for h in self.adjacency.neighbors_vec(entity) {
            if cand.len() >= gather_cap {
                break;
            }
            cand.push(h);
        }
        // Verify (live AND still references entity), stopping at `cap`.
        let mut out: Vec<HyperedgeId> = Vec::with_capacity(cap);
        for h in cand {
            if self
                .current_hyperedge(h)
                .is_some_and(|hr| hr.roles.iter().any(|(_, e)| *e == entity))
            {
                out.push(h);
                if out.len() >= cap {
                    break;
                }
            }
        }
        out
    }

    /// All hyperedges of the given type.
    #[must_use]
    pub fn hyperedges_by_type(&self, type_id: TypeId) -> Vec<HyperedgeId> {
        if !self.type_cluster_served_from_disk() {
            return self.type_cluster.by_type_vec(type_id);
        }
        let key = type_id.get().to_be_bytes();
        let mut cand: HashSet<HyperedgeId> = HashSet::new();
        for f in self.type_cluster_files.values() {
            cand.extend(f.find(&key).into_iter().map(HyperedgeId::from_bytes));
        }
        cand.extend(self.type_cluster.by_type_vec(type_id));
        let mut out: Vec<HyperedgeId> = cand
            .into_iter()
            .filter(|h| self.current_hyperedge(*h).is_some_and(|hr| hr.type_id == type_id))
            .collect();
        out.sort();
        out
    }

    /// Whether hyperedge `hid` is currently clustered under `type_id`.
    /// O(1) reverse-map probe — lets the join executor filter an entity's
    /// adjacency list by edge type without materialising the entire type
    /// bucket into a set per query.
    #[must_use]
    pub fn hyperedge_has_type(&self, hid: HyperedgeId, type_id: TypeId) -> bool {
        if self.type_cluster_served_from_disk() {
            // The hyperedge's type lives on the record itself — verify
            // directly (a point read), no sidecar scan needed.
            return self.current_hyperedge(hid).is_some_and(|h| h.type_id == type_id);
        }
        self.type_cluster.is_type(type_id, hid)
    }

    /// Count of hyperedges of `type_id`. Constant-time index probe; used
    /// by the planner to estimate cardinality without materialising.
    #[must_use]
    pub fn hyperedge_type_count(&self, type_id: TypeId) -> usize {
        if self.type_cluster_served_from_disk() {
            return self.hyperedges_by_type(type_id).len();
        }
        self.type_cluster.count(type_id)
    }

    /// All entities of the given type. O(N) in bucket size.
    #[must_use]
    pub fn entities_by_type(&self, type_id: TypeId) -> Vec<EntityId> {
        if !self.entity_type_served_from_disk() {
            return self.entity_type_cluster.by_type_vec(type_id);
        }
        let key = type_id.get().to_be_bytes();
        let mut cand: HashSet<EntityId> = HashSet::new();
        for f in self.entity_type_files.values() {
            cand.extend(f.find(&key).into_iter().map(EntityId::from_bytes));
        }
        cand.extend(self.entity_type_cluster.by_type_vec(type_id));
        let mut out: Vec<EntityId> = cand
            .into_iter()
            .filter(|e| self.entity_current_type(*e) == Some(type_id))
            .collect();
        out.sort();
        out
    }

    /// Count of entities of `type_id`. Constant-time index probe. Used
    /// by the v3 count-aggregate fast path in the query executor.
    #[must_use]
    pub fn entity_type_count(&self, type_id: TypeId) -> usize {
        if self.entity_type_served_from_disk() {
            return self.entities_by_type(type_id).len();
        }
        self.entity_type_cluster.count(type_id)
    }

    /// Degree of `entity` in the adjacency index — number of hyperedges
    /// that name it in any role. Planner uses this for hyperedge atoms
    /// with at least one role bound to a concrete entity.
    #[must_use]
    pub fn adjacency_degree(&self, entity: EntityId) -> usize {
        if self.adjacency_served_from_disk() {
            return self.hyperedges_for_entity(entity).len();
        }
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
        if !self.property_served_from_disk() {
            return self.property_btree.find(type_id, property_id, value);
        }
        let Some(target) = value_to_index_bytes(value) else {
            return Vec::new();
        };
        let mut cand: HashSet<EntityId> = HashSet::new();
        for f in self.property_index_files.values() {
            cand.extend(f.find(type_id, property_id, &target));
        }
        cand.extend(self.property_btree.find(type_id, property_id, value));
        let mut out: Vec<EntityId> = cand
            .into_iter()
            .filter(|e| {
                self.current_indexed_value(*e, type_id, property_id).as_deref()
                    == Some(target.as_slice())
            })
            .collect();
        out.sort();
        out
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
        if !self.property_served_from_disk() {
            return self.property_btree.range(type_id, property_id, low, high);
        }
        let lo_b = low.and_then(value_to_index_bytes);
        let hi_b = high.and_then(value_to_index_bytes);
        let mut cand: HashSet<EntityId> = HashSet::new();
        for f in self.property_index_files.values() {
            cand.extend(f.range(type_id, property_id, lo_b.as_deref(), hi_b.as_deref()));
        }
        cand.extend(self.property_btree.range(type_id, property_id, low, high));
        let mut out: Vec<EntityId> = cand
            .into_iter()
            .filter(|e| match self.current_indexed_value(*e, type_id, property_id) {
                Some(v) => {
                    lo_b.as_ref().is_none_or(|l| &v >= l)
                        && hi_b.as_ref().is_none_or(|h| &v <= h)
                }
                None => false,
            })
            .collect();
        out.sort();
        out
    }

    /// Top-`k` entities of `type_id` by `property_id`, highest value first.
    /// Bounded ordered scan over the property B-tree — stops at `k` without
    /// materialising the whole column. Lets an application serve "top-N by
    /// X" (e.g. most-cited) without keeping its own sorted copy.
    #[must_use]
    pub fn property_top_k(
        &self,
        type_id: TypeId,
        property_id: PropertyId,
        k: usize,
    ) -> Vec<EntityId> {
        if k == 0 {
            return Vec::new();
        }
        if !self.property_served_from_disk() {
            return self.property_btree.top_k(type_id, property_id, k);
        }
        // Gather top-k candidates from every sidecar + the RAM mirror. A
        // globally top-k entity (by current value) is within top-k of the
        // sidecar that holds its current value, so k-per-source is enough.
        let mut cand: HashSet<EntityId> = HashSet::new();
        for f in self.property_index_files.values() {
            cand.extend(f.top_k(type_id, property_id, k).into_iter().map(|(_, e)| e));
        }
        cand.extend(self.property_btree.top_k(type_id, property_id, k));
        // Verify + rank by the CURRENT value (drops stale-high entries),
        // then take k.
        let mut scored: Vec<(Vec<u8>, EntityId)> = cand
            .into_iter()
            .filter_map(|e| {
                self.current_indexed_value(e, type_id, property_id).map(|v| (v, e))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.truncate(k);
        scored.into_iter().map(|(_, e)| e).collect()
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
        if k == 0 {
            return Vec::new();
        }
        if !self.vector_served_from_disk() {
            return self.vector.search(property_id, query, k, metric);
        }
        // Gather top-k candidates from each sidecar + the RAM mirror. A
        // globally top-k entity (by current embedding) is within top-k of
        // the sidecar holding its current embedding (subset argument), so
        // k-per-source is enough.
        let mut cand: HashSet<EntityId> = HashSet::new();
        for f in self.vector_index_files.values() {
            cand.extend(f.search(property_id, query, k, metric).into_iter().map(|(e, _)| e));
        }
        cand.extend(
            self.vector
                .search(property_id, query, k, metric)
                .into_iter()
                .map(|(e, _)| e),
        );
        // Verify + re-score with the CURRENT embedding (drops tombstoned /
        // superseded / missing), then rank.
        let mut scored: Vec<(f32, EntityId)> = cand
            .into_iter()
            .filter_map(|e| {
                self.current_vector(e, property_id)
                    .filter(|v| v.len() == query.len())
                    .map(|v| (vector_distance(query, &v, metric), e))
            })
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        scored.truncate(k);
        scored.into_iter().map(|(d, e)| (e, d)).collect()
    }

    // -----------------------------------------------------------------
    // Global current-vector snapshot (fast bounded kNN at scale)
    // -----------------------------------------------------------------

    /// Path of the `.vsnap` for `property` (one merged current-vector file,
    /// distinct from the per-SSTable `<seq>.vidx` sidecars).
    fn vsnap_path(&self, property: PropertyId) -> PathBuf {
        self.db.path().join(format!("vsnap-{}.vsnap", property.get()))
    }

    /// Build (or rebuild) the global current-vector snapshot for `property`:
    /// stream every CURRENT entity once, collect its vector into a single
    /// `.vsnap` (the `.vidx` format), then mmap it. Subsequent
    /// [`vector_search_snapshot`] reads ONLY this file — no per-sidecar
    /// fan-out, no per-candidate MVCC verify (the snapshot already holds the
    /// resolved-current vectors). One-time full scan (like a flush), bounded
    /// memory; persisted, so a restart just re-mmaps it. Returns the vector
    /// count. NOTE: read-mostly contract — call again after writes to refresh.
    pub fn build_vector_snapshot(&mut self, property: PropertyId) -> Result<usize, EngineError> {
        let path = self.vsnap_path(property);

        // Pass 1 (counting): find the property's dim (first vector's length)
        // and the number of current vectors at that dim. Bounded — no vectors
        // retained.
        let mut dim = 0usize;
        let mut count = 0usize;
        for item in self.snapshot_iter_streaming(TxId::ACTIVE) {
            let Ok(Record::Entity(e)) = item else { continue };
            for (pid, val) in &e.properties {
                if *pid == property
                    && let Value::Vector(v) = val
                {
                    if dim == 0 {
                        dim = v.len();
                    }
                    if v.len() == dim {
                        count += 1;
                    }
                }
            }
        }
        if count == 0 || dim == 0 {
            // Nothing to index — drop any stale snapshot so search falls back.
            let _ = std::fs::remove_file(&path);
            self.vector_snapshots.remove(&property.get());
            return Ok(0);
        }

        // Pass 2 (streaming write): emit each current vector directly to the
        // .vsnap as it streams — one vector resident at a time, NOT 2×N like
        // the in-RAM builder (keeps the snapshot build under the app RAM cap).
        let entries = self.snapshot_iter_streaming(TxId::ACTIVE).filter_map(move |item| {
            let Ok(Record::Entity(e)) = item else { return None };
            e.properties.iter().find_map(|(pid, val)| match val {
                Value::Vector(v) if *pid == property && v.len() == dim => {
                    Some((e.entity_id, v.clone()))
                }
                _ => None,
            })
        });
        write_vsnap_streaming(&path, property, dim, count, entries)
            .map_err(|e| EngineError::Io(std::io::Error::other(format!("vsnap write {}: {e}", path.display()))))?;

        if let Some(f) = VectorIndexFile::open(&path)
            .map_err(|e| EngineError::Io(std::io::Error::other(format!("vsnap open {}: {e}", path.display()))))?
        {
            self.vector_snapshots.insert(property.get(), f);
        }
        Ok(count)
    }

    /// Open an already-built `.vsnap` for `property` into memory (mmap) if it
    /// exists on disk. Returns whether one was loaded. Cheap — re-mmaps the
    /// file; the vectors stay OS-paged.
    pub fn load_vector_snapshot(&mut self, property: PropertyId) -> Result<bool, EngineError> {
        let path = self.vsnap_path(property);
        match VectorIndexFile::open(&path)
            .map_err(|e| EngineError::Io(std::io::Error::other(format!("vsnap open {}: {e}", path.display()))))?
        {
            Some(f) => {
                self.vector_snapshots.insert(property.get(), f);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Whether a current-vector snapshot is loaded for `property`.
    #[must_use]
    pub fn has_vector_snapshot(&self, property: PropertyId) -> bool {
        self.vector_snapshots.contains_key(&property.get())
    }

    /// k-NN over the global current-vector snapshot — one mmap'd file, no
    /// fan-out, no verify. `None` if no snapshot is loaded for `property`
    /// (caller falls back to [`vector_search`]). Bounded memory + exact (it
    /// brute-forces the snapshot, which holds exactly the current vectors).
    #[must_use]
    pub fn vector_search_snapshot(
        &self,
        property: PropertyId,
        query: &[f32],
        k: usize,
        metric: Distance,
    ) -> Option<Vec<(EntityId, f32)>> {
        self.vector_snapshots
            .get(&property.get())
            .map(|f| f.search(property, query, k, metric))
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

    /// The resolved [`EngineConfig`] this engine is operating under.
    #[must_use]
    pub fn config(&self) -> EngineConfig {
        self.config
    }

    /// Take a consistent **hot backup** of the database into `dest` while
    /// the engine stays open and serving.
    ///
    /// Correctness rests on the LSM's append-only invariant: every file the
    /// active MANIFEST references (`<seq>.ndb` SSTables and their immutable
    /// sidecars) is never mutated after publish, so copying it while writes
    /// continue is safe. The active WAL is copied last; a backup may capture
    /// a torn final WAL record exactly as a crash would, and restore handles
    /// it through the normal `WalRecovery` path — so the backup is always a
    /// recoverable point-in-time image of all *committed* state (no flush
    /// required; un-flushed-but-committed records live in the copied WAL).
    ///
    /// What is copied: `CURRENT`, every `MANIFEST-*`, the at-rest encryption
    /// marker (if present), and — for every SSTable the manifest references
    /// plus the active WAL — the data file and all sidecars sharing its
    /// `<seq>` stem (`.idx`, `.bloom`, `.pidx`, `.vidx`, …). Restore is
    /// simply: point a new [`Engine::open`] at `dest`.
    ///
    /// `dest` is created if absent. An existing `dest` is written into (files
    /// with colliding names are overwritten); callers wanting a clean target
    /// should pass an empty directory.
    pub fn backup_to(&self, dest: &Path) -> Result<BackupStats, EngineError> {
        let src = self.db.path().to_path_buf();
        std::fs::create_dir_all(dest)?;

        // Stems whose `<stem>.*` siblings must be carried over: every live
        // SSTable plus the active WAL.
        let manifest = self.db.manifest();
        let mut stems: std::collections::HashSet<String> = manifest
            .sstables
            .iter()
            .map(|e| format!("{:06}", e.file_seq))
            .collect();
        if manifest.active_wal_seq != 0 {
            stems.insert(format!("{:06}", manifest.active_wal_seq));
        }

        let mut stats = BackupStats::default();
        for entry in std::fs::read_dir(&src)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // The stem is the filename up to the first '.', matching how
            // SSTables (`000007.ndb`) and their sidecars (`000007.idx`,
            // `000007.bloom`, …) are named.
            let stem = name.split('.').next().unwrap_or("");
            let take = name == crate::db::CURRENT_FILE
                || name.starts_with(crate::db::MANIFEST_PREFIX)
                || name == ENCRYPTION_MARKER_FILENAME
                || stems.contains(stem);
            if !take {
                continue;
            }
            let bytes = std::fs::copy(entry.path(), dest.join(name))?;
            stats.files_copied += 1;
            stats.bytes_copied += bytes;
            if name.ends_with(SSTABLE_FILENAME_SUFFIX) {
                stats.sstables += 1;
            }
        }
        // fsync the backup directory so the copied set is durable.
        if let Ok(d) = std::fs::File::open(dest) {
            let _ = d.sync_all();
        }
        Ok(stats)
    }

    /// Per-index resident heap estimate. Diagnostic — walks the indexes
    /// (O(N)); use for the RAM-vs-DB-size baseline curve, not the hot
    /// path. See [`IndexMemoryStats`].
    #[must_use]
    pub fn index_memory_stats(&self) -> IndexMemoryStats {
        IndexMemoryStats {
            lookup_key: self.lookup_key.heap_bytes(),
            adjacency: self.adjacency.heap_bytes(),
            type_cluster: self.type_cluster.heap_bytes(),
            entity_type_cluster: self.entity_type_cluster.heap_bytes(),
            vector: self.vector.heap_bytes(),
            property_btree: self.property_btree.heap_bytes(),
            memtable: usize::try_from(self.memtable.size_bytes()).unwrap_or(usize::MAX),
        }
    }

    /// Build + write the `<seq>.pidx` property-index sidecar for a
    /// just-published SSTable, covering the registered `(type, prop)`
    /// pairs. No-op when nothing is registered or no indexed values are
    /// present. Indexes every Entity version in the SSTable; read-time
    /// MVCC verification (Phase 1c) drops stale/superseded candidates, so
    /// over-inclusion is safe. Best-effort: a sidecar write failure is
    /// surfaced (callers treat it as an engine error), but a *missing*
    /// sidecar at read time simply falls back to a RAM rebuild.
    fn write_index_sidecars(&self, reader: &SSTableReader) -> Result<(), EngineError> {
        // Sidecars are a low-RAM-mode artifact; default mode writes none
        // (no overhead). A DB must be created/opened with `mmap_indexes`
        // for its flushes/compactions to emit sidecars.
        if !self.config.mmap_indexes {
            return Ok(());
        }
        let want_property = self.property_btree.has_registrations();
        let want_vector = self.vector.has_registrations();
        let want_lookup = self.lookup_key.has_registrations();
        let mut pbuilder = PropertyIndexBuilder::new();
        let mut vbuilder = VectorIndexBuilder::new();
        let mut adj = IdListIndexBuilder::new(ADJ_MAGIC);
        let mut tyc = IdListIndexBuilder::new(TYC_MAGIC);
        let mut etc = IdListIndexBuilder::new(ETC_MAGIC);
        let mut lkp = IdListIndexBuilder::new(LKP_MAGIC);
        let mut meta_ts: Vec<(u64, i64)> = Vec::new();
        let mut meta_ret: Vec<(u32, u8, u32)> = Vec::new();
        let path = reader.path();
        for item in reader.iter() {
            let (rec, _) = item?;
            match &rec {
                Record::TxTimestamp(t) => meta_ts.push((t.tx_id.get(), t.timestamp_us)),
                Record::RetentionPolicy(rp) => {
                    meta_ret.push((rp.type_id.get(), rp.policy_kind, rp.keep_last_n));
                }
                _ => {}
            }
            match &rec {
                Record::Entity(e) => {
                    // entity_type_cluster: type → entity (always indexed).
                    etc.observe(&e.type_id.get().to_be_bytes(), *e.entity_id.as_bytes());
                    for (prop, val) in &e.properties {
                        if want_property
                            && self.property_btree.is_registered(e.type_id, *prop)
                            && let Some(bytes) = value_to_index_bytes(val)
                        {
                            pbuilder.observe(e.type_id, *prop, &bytes, e.entity_id);
                        }
                        if want_vector
                            && self.vector.is_registered(*prop)
                            && let Value::Vector(v) = val
                        {
                            vbuilder.observe(*prop, e.entity_id, v);
                        }
                        if want_lookup
                            && self.lookup_key.is_registered(*prop)
                            && let Some(bytes) = lookup_value_to_index_bytes(val)
                        {
                            let mut key = prop.get().to_be_bytes().to_vec();
                            key.extend_from_slice(&bytes);
                            lkp.observe(&key, *e.entity_id.as_bytes());
                        }
                    }
                }
                Record::HyperEdge(h) => {
                    // type_cluster: type → hyperedge; adjacency: entity →
                    // hyperedge (per role-filler). Always indexed.
                    tyc.observe(&h.type_id.get().to_be_bytes(), *h.hyperedge_id.as_bytes());
                    for (_role, entity) in &h.roles {
                        adj.observe(entity.as_bytes(), *h.hyperedge_id.as_bytes());
                    }
                }
                _ => {}
            }
        }
        if want_property && !pbuilder.is_empty() {
            pbuilder
                .finish(&pidx_sidecar_path_for(path))
                .map_err(|e| std::io::Error::other(format!("property index sidecar: {e}")))?;
        }
        if want_vector && !vbuilder.is_empty() {
            vbuilder
                .finish(&vidx_sidecar_path_for(path))
                .map_err(|e| std::io::Error::other(format!("vector index sidecar: {e}")))?;
        }
        let id_lists = [
            (adj, ADJ_EXT, "adjacency"),
            (tyc, TYC_EXT, "type_cluster"),
            (etc, ETC_EXT, "entity_type"),
            (lkp, LKP_EXT, "lookup_key"),
        ];
        for (builder, ext, label) in id_lists {
            if !builder.is_empty() {
                builder
                    .finish(&idl_sidecar_path_for(path, ext))
                    .map_err(|e| std::io::Error::other(format!("{label} index sidecar: {e}")))?;
            }
        }
        // Metadata sidecar (tx timestamps + retention) so a low-RAM open
        // can skip scanning this SSTable yet still resolve as-of-timestamp
        // + honour retention.
        if !meta_ts.is_empty() || !meta_ret.is_empty() {
            let bytes = encode_meta(&meta_ts, &meta_ret);
            let mp = idl_sidecar_path_for(path, META_EXT);
            let tmp = mp.with_extension("meta.tmp");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &mp)?;
        }
        Ok(())
    }

    /// Populate `commit_timestamps` + `retention` from the per-SSTable
    /// `.meta` sidecars (low-RAM mode). Lets `rebuild_indexes` skip the
    /// full SSTable scan while keeping as-of-timestamp + retention correct.
    fn load_meta_sidecars(&mut self) {
        if !self.config.mmap_indexes {
            return;
        }
        let paths: Vec<PathBuf> = self.sstables.iter().map(|s| s.path().to_path_buf()).collect();
        for p in paths {
            let mp = idl_sidecar_path_for(&p, META_EXT);
            let Ok(bytes) = std::fs::read(&mp) else { continue };
            if let Some((ts, ret)) = decode_meta(&bytes) {
                for (tx, t) in ts {
                    self.commit_timestamps.insert(TxId::new(tx), t);
                }
                for (ty, kind, n) in ret {
                    if let Some(p) = decode_retention_policy(kind, n) {
                        self.retention.insert(TypeId::new(ty), p);
                    }
                }
            }
        }
    }

    /// Open each SSTable's `.pidx` sidecar into `property_index_files`.
    /// Only under `config.mmap_indexes`; missing/corrupt sidecars are
    /// skipped (those SSTables fall back to the in-RAM property mirror).
    fn load_property_index_sidecars(&mut self) {
        self.property_index_files.clear();
        if !self.config.mmap_indexes {
            return;
        }
        for sst in &self.sstables {
            let pidx = pidx_sidecar_path_for(sst.path());
            match PropertyIndexFile::open(&pidx) {
                Ok(Some(f)) => {
                    self.property_index_files.insert(sst.path().to_path_buf(), f);
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!(
                        "ndb-engine: property-index sidecar {} unreadable ({e}); \
                         falling back to RAM mirror",
                        pidx.display()
                    );
                }
            }
        }
    }

    /// Whether `(type, prop)` queries should be served from on-disk
    /// sidecars + verification (low-RAM mode with at least one sidecar)
    /// rather than purely from the in-RAM property B-tree.
    fn property_served_from_disk(&self) -> bool {
        self.config.mmap_indexes && !self.property_index_files.is_empty()
    }

    /// Open each SSTable's `.vidx` sidecar into `vector_index_files`. Only
    /// under `config.mmap_indexes`; missing/corrupt → RAM-mirror fallback.
    fn load_vector_index_sidecars(&mut self) {
        self.vector_index_files.clear();
        if !self.config.mmap_indexes {
            return;
        }
        for sst in &self.sstables {
            let vidx = vidx_sidecar_path_for(sst.path());
            match VectorIndexFile::open(&vidx) {
                Ok(Some(f)) => {
                    self.vector_index_files.insert(sst.path().to_path_buf(), f);
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!(
                        "ndb-engine: vector-index sidecar {} unreadable ({e}); \
                         falling back to RAM mirror",
                        vidx.display()
                    );
                }
            }
        }
    }

    /// Whether vector search should gather from on-disk sidecars +
    /// verification rather than purely from the in-RAM vector index.
    fn vector_served_from_disk(&self) -> bool {
        self.config.mmap_indexes && !self.vector_index_files.is_empty()
    }

    fn adjacency_served_from_disk(&self) -> bool {
        self.config.mmap_indexes && !self.adjacency_files.is_empty()
    }
    fn type_cluster_served_from_disk(&self) -> bool {
        self.config.mmap_indexes && !self.type_cluster_files.is_empty()
    }
    fn entity_type_served_from_disk(&self) -> bool {
        self.config.mmap_indexes && !self.entity_type_files.is_empty()
    }
    fn lookup_served_from_disk(&self) -> bool {
        self.config.mmap_indexes && !self.lookup_key_files.is_empty()
    }

    /// Current live hyperedge record at the latest snapshot, or `None`.
    fn current_hyperedge(&self, hid: HyperedgeId) -> Option<HyperEdgeRecord> {
        let snap = TxId::new(self.db.manifest().last_tx_id);
        match self.snapshot_read(&hid.into_uuid(), snap) {
            Ok(Resolved::Live(Record::HyperEdge(h))) => Some(h),
            _ => None,
        }
    }

    /// Current type of a live entity at the latest snapshot, or `None`.
    fn entity_current_type(&self, eid: EntityId) -> Option<TypeId> {
        let snap = TxId::new(self.db.manifest().last_tx_id);
        match self.snapshot_read(&eid.into_uuid(), snap) {
            Ok(Resolved::Live(Record::Entity(e))) => Some(e.type_id),
            _ => None,
        }
    }

    /// Whether a live entity's `property_id` currently equals `want_bytes`
    /// (lookup-key encoding). The verify step behind on-disk lookup-key.
    fn entity_lookup_matches(
        &self,
        eid: EntityId,
        property_id: PropertyId,
        want_bytes: &[u8],
    ) -> bool {
        let snap = TxId::new(self.db.manifest().last_tx_id);
        match self.snapshot_read(&eid.into_uuid(), snap) {
            Ok(Resolved::Live(Record::Entity(e))) => e
                .properties
                .iter()
                .find(|(p, _)| *p == property_id)
                .and_then(|(_, v)| lookup_value_to_index_bytes(v))
                .is_some_and(|b| b == want_bytes),
            _ => false,
        }
    }

    /// Resolve an entity's *current* embedding for `property_id` at the
    /// latest snapshot, or `None` if deleted / lacking the property. The
    /// MVCC verification step behind on-disk vector search.
    fn current_vector(&self, entity: EntityId, property_id: PropertyId) -> Option<Vec<f32>> {
        let snap = TxId::new(self.db.manifest().last_tx_id);
        match self.snapshot_read(&entity.into_uuid(), snap) {
            Ok(Resolved::Live(Record::Entity(e))) => {
                e.properties.iter().find(|(p, _)| *p == property_id).and_then(
                    |(_, v)| {
                        if let Value::Vector(vec) = v {
                            Some(vec.clone())
                        } else {
                            None
                        }
                    },
                )
            }
            _ => None,
        }
    }

    /// Rebuild the in-RAM vector index to mirror only data WITHOUT a `.vidx`
    /// sidecar (sidecar-less SSTables + the memtable). Called after a flush
    /// under `mmap_indexes` so just-flushed embeddings leave RAM.
    fn refresh_vector_ram_mirror(&mut self) -> Result<(), EngineError> {
        self.vector.clear();
        for sst in &mut self.sstables {
            if self.vector_index_files.contains_key(sst.path()) {
                continue;
            }
            for item in sst.iter() {
                let (rec, _) = item?;
                self.vector.apply(&rec, record_index_tx(&rec));
            }
        }
        for (_k, rec) in self.memtable.iter() {
            self.vector.apply(rec, record_index_tx(rec));
        }
        Ok(())
    }

    /// Open the four id-list sidecars for every SSTable (under
    /// `config.mmap_indexes`). Collects SSTable paths first to avoid
    /// borrowing `self.sstables` while inserting into the file maps.
    fn load_id_list_sidecars(&mut self) {
        self.adjacency_files.clear();
        self.type_cluster_files.clear();
        self.entity_type_files.clear();
        self.lookup_key_files.clear();
        if !self.config.mmap_indexes {
            return;
        }
        let paths: Vec<PathBuf> = self.sstables.iter().map(|s| s.path().to_path_buf()).collect();
        for p in paths {
            self.insert_id_list_sidecars(&p);
        }
    }

    /// Open + register the four id-list sidecars for one SSTable path.
    fn insert_id_list_sidecars(&mut self, sst_path: &Path) {
        if let Ok(Some(f)) = IdListIndexFile::open(&idl_sidecar_path_for(sst_path, ADJ_EXT), ADJ_MAGIC) {
            self.adjacency_files.insert(sst_path.to_path_buf(), f);
        }
        if let Ok(Some(f)) = IdListIndexFile::open(&idl_sidecar_path_for(sst_path, TYC_EXT), TYC_MAGIC) {
            self.type_cluster_files.insert(sst_path.to_path_buf(), f);
        }
        if let Ok(Some(f)) = IdListIndexFile::open(&idl_sidecar_path_for(sst_path, ETC_EXT), ETC_MAGIC) {
            self.entity_type_files.insert(sst_path.to_path_buf(), f);
        }
        if let Ok(Some(f)) = IdListIndexFile::open(&idl_sidecar_path_for(sst_path, LKP_EXT), LKP_MAGIC) {
            self.lookup_key_files.insert(sst_path.to_path_buf(), f);
        }
    }

    /// Rebuild the four in-RAM id-list indexes to mirror only data WITHOUT
    /// a sidecar (sidecar-less SSTables + the memtable). Each index skips
    /// SSTables it has on disk. Bounded footprint in steady state.
    fn refresh_id_list_ram_mirrors(&mut self) -> Result<(), EngineError> {
        self.adjacency.clear();
        self.type_cluster.clear();
        self.entity_type_cluster.clear();
        self.lookup_key.clear();
        for sst in &mut self.sstables {
            let path = sst.path();
            let adj_disk = self.adjacency_files.contains_key(path);
            let tyc_disk = self.type_cluster_files.contains_key(path);
            let etc_disk = self.entity_type_files.contains_key(path);
            let lkp_disk = self.lookup_key_files.contains_key(path);
            for item in sst.iter() {
                let (rec, _) = item?;
                let tx = record_index_tx(&rec);
                if !adj_disk {
                    self.adjacency.apply(&rec, tx);
                }
                if !tyc_disk {
                    self.type_cluster.apply(&rec, tx);
                }
                if !etc_disk {
                    self.entity_type_cluster.apply(&rec, tx);
                }
                if !lkp_disk {
                    self.lookup_key.apply(&rec, tx);
                }
            }
        }
        for (_k, rec) in self.memtable.iter() {
            let tx = record_index_tx(rec);
            self.adjacency.apply(rec, tx);
            self.type_cluster.apply(rec, tx);
            self.entity_type_cluster.apply(rec, tx);
            self.lookup_key.apply(rec, tx);
        }
        Ok(())
    }

    /// Resolve an entity's *current* order-preserving value bytes for
    /// `(type_id, property_id)` at the latest snapshot, or `None` if the
    /// entity is deleted, not of `type_id`, or lacks the property. The
    /// MVCC verification step behind every on-disk property query.
    fn current_indexed_value(
        &self,
        entity: EntityId,
        type_id: TypeId,
        property_id: PropertyId,
    ) -> Option<Vec<u8>> {
        let snap = TxId::new(self.db.manifest().last_tx_id);
        match self.snapshot_read(&entity.into_uuid(), snap) {
            Ok(Resolved::Live(Record::Entity(e))) if e.type_id == type_id => e
                .properties
                .iter()
                .find(|(p, _)| *p == property_id)
                .and_then(|(_, v)| value_to_index_bytes(v)),
            _ => None,
        }
    }

    /// Rebuild the in-RAM property B-tree to mirror only data WITHOUT an
    /// on-disk sidecar (sidecar-less SSTables + the memtable). Called after
    /// a flush under `mmap_indexes` so the just-flushed (now sidecar-backed)
    /// entries leave RAM — this is what keeps the property index resident
    /// footprint bounded by the memtable, not the whole DB. In steady state
    /// (every SSTable has a sidecar) the result is just the memtable.
    fn refresh_property_ram_mirror(&mut self) -> Result<(), EngineError> {
        self.property_btree.clear();
        for sst in &mut self.sstables {
            if self.property_index_files.contains_key(sst.path()) {
                continue;
            }
            for item in sst.iter() {
                let (rec, _) = item?;
                self.property_btree.apply(&rec, record_index_tx(&rec));
            }
        }
        for (_k, rec) in self.memtable.iter() {
            self.property_btree.apply(rec, record_index_tx(rec));
        }
        Ok(())
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
        &self,
        uuid: &uuid::Uuid,
        snapshot: TxId,
    ) -> Result<Resolved<Record>, EngineError> {
        // One key allocation, reused across all (kind, layer) probes by
        // mutating only the `kind` discriminant — the 16-byte primary is
        // identical for the three kinds, so allocating it per kind/per
        // SSTable (the old `uuid.as_bytes().to_vec()` in each loop body)
        // was pure waste. At 64 threads this also cuts allocator-lock
        // pressure, which is what bites concurrent point/pattern lookups.
        let mut key = SSTableKey {
            kind: 0,
            primary: uuid.as_bytes().to_vec(),
        };

        // SSTable hits must be owned (deserialized out of the file), so
        // hold them in a local buffer we can borrow alongside the
        // memtable's borrowed version slices. find_all() probes the
        // block-index sidecar when present (O(log N) seek + ≤ block_size
        // scan) and linear-scans the whole file otherwise. Reads go
        // through `&self` (mmap / decrypted heap buffer; both immutable
        // post-open), which is what lets `RwLock<Engine>` parallelise
        // concurrent point lookups.
        let mut sstable_owned: Vec<Record> = Vec::new();
        for sst in &self.sstables {
            for kind in [
                crate::record::RecordKind::Entity,
                crate::record::RecordKind::HyperEdge,
                crate::record::RecordKind::Tombstone,
            ] {
                key.kind = kind.as_byte();
                sstable_owned.extend(sst.find_all(&key)?);
            }
        }

        // Resolve over BORROWED candidates — memtable version slices are
        // referenced in place (no per-version clone), SSTable records
        // borrowed from the local buffer. Newest layer first: memtable,
        // then SSTables. Only the winning record is cloned, instead of
        // cloning every candidate and then the winner again.
        let mut refs: Vec<&Record> = Vec::new();
        for kind in [
            crate::record::RecordKind::Entity,
            crate::record::RecordKind::HyperEdge,
            crate::record::RecordKind::Tombstone,
        ] {
            key.kind = kind.as_byte();
            if let Some(vs) = self.memtable.versions(&key) {
                refs.extend(vs.iter());
            }
        }
        refs.extend(sstable_owned.iter());

        Ok(match resolve_iter(refs, snapshot) {
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
        // Pre-scan tombstones across every source — entity/hyperedge
        // records and their tombstones live under different SSTableKey
        // kind bytes, so the merge sort never groups them together.
        // Without this pre-pass the streaming pump would emit deleted
        // records (the per-key resolve only sees same-kind versions).
        // Tombstones are tiny (target_uuid + tx_id_supersede) so the
        // pre-scan cost stays well below the entity-iteration cost it
        // gates.
        let mut tombstones: std::collections::HashMap<uuid::Uuid, TxId> =
            std::collections::HashMap::new();
        // Memtable tombstones.
        for (k, r) in self.memtable.iter() {
            if k.kind == crate::record::RecordKind::Tombstone.as_byte()
                && let Record::Tombstone(t) = r
            {
                tombstones
                    .entry(t.target_id)
                    .and_modify(|cur| {
                        if t.tx_id_supersede > *cur {
                            *cur = t.tx_id_supersede;
                        }
                    })
                    .or_insert(t.tx_id_supersede);
            }
        }
        // SSTable tombstones — block-index sidecar gives O(log n) seek
        // when present; otherwise the iter call still covers it.
        for sst in &self.sstables {
            for item in sst.iter() {
                let Ok((rec, _)) = item else { continue };
                if let Record::Tombstone(t) = &rec {
                    tombstones
                        .entry(t.target_id)
                        .and_modify(|cur| {
                            if t.tx_id_supersede > *cur {
                                *cur = t.tx_id_supersede;
                            }
                        })
                        .or_insert(t.tx_id_supersede);
                }
            }
        }
        SnapshotStream::new(sources, snapshot).with_tombstones(tombstones)
    }

    /// Iterate every record visible at `snapshot`, in (kind, primary)
    /// order, deduplicating across memtable + SSTables. Useful for scans.
    /// O(N) — v1 has no block index.
    ///
    /// Materialises the full result set in a `Vec`. For very large scans,
    /// prefer [`Self::snapshot_iter_streaming`] which yields records one
    /// at a time without buffering.
    pub fn snapshot_iter(&self, snapshot: TxId) -> Result<Vec<Record>, EngineError> {
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
        let mut writer = SSTableWriter::create_with_cipher(&sst_path, self.cipher.clone())?;
        self.memtable.flush_into(&mut writer)?;
        writer.finish()?;

        // Step 3: mint new WAL.
        let new_wal_seq = self.db.allocate_file_seq();
        let new_wal_path = wal_path(self.db.path(), new_wal_seq);
        let new_wal = WriteAheadLog::create_with_cipher(&new_wal_path, self.cipher.clone())?;

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
        let reader = SSTableReader::open_with_cipher(&sst_path, self.cipher.clone())?;
        // Index sidecars (low-RAM core): built from the freshly written
        // SSTable's contents. Read path uses them only under
        // `config.mmap_indexes`; default mode ignores them.
        self.write_index_sidecars(&reader)?;
        self.sstables.insert(0, reader);
        // Under mmap mode: register the new sidecars and drop the just-
        // flushed entries from the RAM mirrors (bounded footprint).
        if self.config.mmap_indexes {
            if let Ok(Some(f)) = PropertyIndexFile::open(&pidx_sidecar_path_for(&sst_path)) {
                self.property_index_files.insert(sst_path.clone(), f);
            }
            if let Ok(Some(f)) = VectorIndexFile::open(&vidx_sidecar_path_for(&sst_path)) {
                self.vector_index_files.insert(sst_path.clone(), f);
            }
            self.insert_id_list_sidecars(&sst_path);
            self.refresh_property_ram_mirror()?;
            self.refresh_vector_ram_mirror()?;
            self.refresh_id_list_ram_mirrors()?;
        }

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
        let mut writer = SSTableWriter::create_with_cipher(&new_path, self.cipher.clone())?;
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
        let reader = SSTableReader::open_with_cipher(&new_path, self.cipher.clone())?;
        // Index sidecars for the compacted SSTable (low-RAM core).
        self.write_index_sidecars(&reader)?;
        self.sstables.clear();
        self.sstables.push(reader);

        // Step 7: remove old files (best-effort). Also remove the
        // companion `<seq>.idx` block-index and `<seq>.pidx` property-index
        // sidecars if they exist.
        for old_seq in old_sstable_seqs {
            let p = sstable_path(self.db.path(), old_seq);
            let _ = std::fs::remove_file(&p);
            let _ = std::fs::remove_file(crate::block_index::sidecar_path_for(&p));
            let _ = std::fs::remove_file(crate::bloom::sidecar_path_for(&p));
            let _ = std::fs::remove_file(pidx_sidecar_path_for(&p));
            let _ = std::fs::remove_file(vidx_sidecar_path_for(&p));
            for ext in [ADJ_EXT, TYC_EXT, ETC_EXT, LKP_EXT, META_EXT] {
                let _ = std::fs::remove_file(idl_sidecar_path_for(&p, ext));
            }
        }

        // Reload sidecars for the (now single) compacted SSTable before the
        // rebuild, so rebuild_indexes skips data served from disk and the
        // RAM mirrors end up holding only the memtable.
        self.load_property_index_sidecars();
        self.load_vector_index_sidecars();
        self.load_id_list_sidecars();
        // Rebuild indexes since we dropped tombstoned records.
        self.rebuild_indexes()?;

        Ok(CompactionStats {
            records_in,
            records_out,
            sstables_in,
            new_sstable_seq: Some(new_seq),
        })
    }

    /// Migrate this database between encryption states.
    ///
    /// Covers the three transitions:
    /// - **Plaintext → encrypted**: no marker on disk, `new_cipher = Some(_)`.
    /// - **Encrypted → encrypted (new key)**: marker on disk, `new_cipher = Some(_)` with a different fingerprint.
    /// - **Encrypted → plaintext**: marker on disk, `new_cipher = None`.
    ///
    /// Idempotent — calling with a cipher whose fingerprint matches the
    /// current state returns `Ok(zero-stats)` and doesn't touch disk.
    ///
    /// ## Crash safety
    ///
    /// 1. Memtable is flushed to a new SSTable before the migration
    ///    starts — every record now lives in an SSTable, so the WAL
    ///    holds nothing except the rotation residue.
    /// 2. A transient `.encryption.next` marker is written before any
    ///    file rewrite begins. Its presence on next `Engine::open` is
    ///    diagnosed as a crashed migration; the engine refuses to open
    ///    silently rather than risk reading some files with the wrong
    ///    cipher.
    /// 3. Each SSTable + the new WAL use the existing
    ///    write-temp-then-rename idiom, so any individual file is
    ///    atomically either old-cipher or new-cipher; never half.
    /// 4. The marker flip is the very last step.
    ///
    /// ## Holds an exclusive `&mut Engine`
    ///
    /// No concurrent reads or writes are possible during the migration.
    /// `SharedEngine` callers are responsible for releasing any active
    /// snapshots before invoking this method.
    pub fn reencrypt(&mut self, new_cipher: Option<&Cipher>) -> Result<MigrationStats, EngineError> {
        // Idempotent: same fingerprint = no-op.
        let cur_fp = self.cipher.as_ref().map(Cipher::fingerprint);
        let new_fp = new_cipher.map(Cipher::fingerprint);
        if cur_fp == new_fp {
            return Ok(MigrationStats::default());
        }

        let next_marker_path = self.db.path().join(ENCRYPTION_MIGRATION_FILENAME);
        if next_marker_path.exists() {
            return Err(EngineError::EncryptionMigrationIncomplete {
                detail: "another migration is already pending — recover or remove the marker first"
                    .into(),
            });
        }

        // Step 1: flush memtable so all data is in SSTables.
        self.flush()?;

        // Step 2: write the transient marker. Contains the target
        // marker bytes (or empty for plaintext target).
        let next_bytes: Vec<u8> = new_cipher
            .map(|c| EncryptionMarker::new(c, DEFAULT_CHUNK_SIZE).encode().to_vec())
            .unwrap_or_default();
        std::fs::write(&next_marker_path, &next_bytes)?;

        // Step 3: rewrite each SSTable. Drop existing readers first so
        // the temp-then-rename inside the writer can replace files.
        self.sstables.clear();
        let mut sstables_rewritten = 0usize;
        let mut bytes_rewritten: u64 = 0;
        let sst_seqs: Vec<u64> = self
            .db
            .manifest()
            .sstables
            .iter()
            .map(|e| e.file_seq)
            .collect();
        let new_cipher_owned = new_cipher.cloned();
        for seq in &sst_seqs {
            let path = sstable_path(self.db.path(), *seq);
            let original_len = std::fs::metadata(&path).map_or(0, |m| m.len());

            // Read all records under the OLD cipher.
            let reader = SSTableReader::open_with_cipher(&path, self.cipher.clone())?;
            let count = usize::try_from(reader.footer().record_count).unwrap_or(usize::MAX);
            let mut records: Vec<Record> = Vec::with_capacity(count);
            for item in reader.iter() {
                let (rec, _) = item?;
                records.push(rec);
            }
            drop(reader);

            // Write under the NEW cipher. SSTableWriter's
            // write-temp-then-rename handles the atomic replace.
            let mut writer =
                SSTableWriter::create_with_cipher(&path, new_cipher_owned.clone())?;
            for r in &records {
                writer.append(r)?;
            }
            writer.finish()?;

            bytes_rewritten += original_len;
            sstables_rewritten += 1;
        }

        // Step 4: rewrite the WAL under the new cipher. The current
        // WAL is empty (flush() just rotated to a fresh segment), but
        // it was created under the OLD cipher; we need to swap it for
        // one created under the NEW cipher. Allocate a new seq.
        let old_wal_seq = self.db.manifest().active_wal_seq;
        let old_wal_path = wal_path(self.db.path(), old_wal_seq);
        let old_wal_len = std::fs::metadata(&old_wal_path).map_or(0, |m| m.len());
        bytes_rewritten += old_wal_len;

        // Drop the current WAL writer so we can delete the file.
        if let Some(w) = self.wal.take() {
            let _ = w.close();
        }
        let new_wal_seq = self.db.allocate_file_seq();
        let new_wal_path = wal_path(self.db.path(), new_wal_seq);
        let new_wal =
            WriteAheadLog::create_with_cipher(&new_wal_path, new_cipher_owned.clone())?;
        let mut manifest = self.db.manifest().clone();
        manifest.active_wal_seq = new_wal_seq;
        self.db.write_manifest(manifest)?;
        let _ = std::fs::remove_file(&old_wal_path);
        self.wal = Some(new_wal);

        // Step 5: flip the marker. Write the new permanent marker (or
        // delete the old one for plaintext target), then remove the
        // transient `.encryption.next`.
        match new_cipher {
            Some(c) => {
                let marker = EncryptionMarker::new(c, DEFAULT_CHUNK_SIZE);
                write_encryption_marker(self.db.path(), &marker)?;
            }
            None => {
                let _ = std::fs::remove_file(self.db.path().join(ENCRYPTION_MARKER_FILENAME));
            }
        }
        std::fs::remove_file(&next_marker_path)?;

        // Step 6: switch the in-memory cipher + reopen SSTable readers
        // under the new cipher.
        self.cipher = new_cipher_owned;
        let entries = self.db.manifest().sstables.clone();
        for entry in &entries {
            let p = sstable_path(self.db.path(), entry.file_seq);
            self.sstables
                .push(SSTableReader::open_with_cipher(&p, self.cipher.clone())?);
        }

        Ok(MigrationStats {
            sstables_rewritten,
            wal_segments_rewritten: 1,
            bytes_rewritten,
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
            self.engine.entity_type_cluster.apply(&r, self.tx_id);
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
    /// Pre-scanned tombstone map: `target_id → latest tx_id_supersede`.
    /// Entity / hyperedge records are emitted only if either (a) no
    /// tombstone exists for their primary id, or (b) the tombstone's
    /// supersede tx is greater than the requested snapshot — i.e. the
    /// delete happened in the future from the snapshot's perspective.
    /// Populated via [`Self::with_tombstones`] at stream construction.
    tombstones: std::collections::HashMap<uuid::Uuid, TxId>,
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
            tombstones: std::collections::HashMap::new(),
        }
    }

    /// Attach a pre-scanned tombstone map. Emitted only when the
    /// streaming iterator sees an entity/hyperedge record whose
    /// primary id matches a tombstone whose `tx_id_supersede` is ≤
    /// the snapshot — that's the MVCC "deleted" condition that
    /// `snapshot_read` already enforces across kinds.
    fn with_tombstones(
        mut self,
        tombstones: std::collections::HashMap<uuid::Uuid, TxId>,
    ) -> Self {
        self.tombstones = tombstones;
        self
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
                // Cross-kind tombstone check — entity/hyperedge records
                // and their tombstones live under different SSTableKey
                // kind bytes, so the per-key resolve above never saw
                // the tombstone. Drop the record if a tombstone for the
                // same primary id is visible at this snapshot.
                let target = match r {
                    Record::Entity(e)    => Some(e.entity_id.into_uuid()),
                    Record::HyperEdge(h) => Some(h.hyperedge_id.into_uuid()),
                    _ => None,
                };
                if let Some(uuid) = target
                    && let Some(supersede) = self.tombstones.get(&uuid)
                    && *supersede <= self.snapshot
                {
                    // Tombstoned at or before snapshot — skip.
                    continue;
                }
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

/// The tx id an index uses as the out-of-order watermark for a record.
fn record_index_tx(rec: &Record) -> TxId {
    match rec {
        Record::Entity(e) => e.tx_id_assert,
        Record::HyperEdge(h) => h.tx_id_assert,
        Record::Tombstone(t) => t.tx_id_supersede,
        Record::TxTimestamp(t) => t.tx_id,
        _ => TxId::new(0),
    }
}

/// Replay every clean WAL record into the memtable. Returns the safe
/// truncate boundary AND the maximum `effective_tx` seen during replay.
///
/// The max-tx return is critical: the previous MANIFEST's `last_tx_id` was
/// persisted at the last flush. Any commits since then are in the WAL but
/// not the MANIFEST. Without reconciling, a snapshot read at
/// `manifest.last_tx_id` would treat the replayed records as invisible.
fn replay_wal_into(
    path: &Path,
    memtable: &mut Memtable,
    cipher: Option<Cipher>,
) -> Result<(u64, u64), EngineError> {
    if !path.exists() {
        return Ok((0, 0));
    }
    let mut reader = WalReader::open_with_cipher(path, cipher)?;
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
// Encryption marker resolution
// ---------------------------------------------------------------------------

/// Read the encryption marker file at `<db>/.encryption`, if any.
fn read_encryption_marker(db_dir: &Path) -> Result<Option<EncryptionMarker>, EngineError> {
    let p = db_dir.join(ENCRYPTION_MARKER_FILENAME);
    match std::fs::read(&p) {
        Ok(bytes) => Ok(Some(EncryptionMarker::decode(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Atomic write of the marker: write-tmp + rename. Keeps the directory
/// in a consistent state across crashes — either the old marker (or
/// none) is visible, or the new marker is.
fn write_encryption_marker(db_dir: &Path, marker: &EncryptionMarker) -> Result<(), EngineError> {
    let final_path = db_dir.join(ENCRYPTION_MARKER_FILENAME);
    let mut tmp_name = final_path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = final_path.with_file_name(tmp_name);
    let bytes = marker.encode();
    std::fs::write(&tmp_path, bytes)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// Reconcile an explicitly-supplied cipher hint against the on-disk
/// marker. Returns the cipher to use for I/O (or `None` for plaintext).
///
/// Matrix:
///
/// | hint | marker | outcome |
/// |------|--------|---------|
/// | None | absent | `Ok(None)` — plaintext database |
/// | None | present | `Err(EncryptionKeyMismatch)` — DB is encrypted, no key supplied |
/// | Some | absent | `Err(EncryptionKeyMismatch)` — refuse implicit migration; use `Engine::reencrypt` |
/// | Some | present + match | `Ok(Some(cipher))` |
/// | Some | present + mismatch | `Err(EncryptionKeyMismatch)` |
fn resolve_cipher_against_marker(
    db_dir: &Path,
    hint: Option<Cipher>,
) -> Result<Option<Cipher>, EngineError> {
    // Pending migration marker → refuse to open. The database may be
    // in a mixed-cipher state where some SSTables are under the old
    // key and others under the new. Manual recovery is required.
    if db_dir.join(ENCRYPTION_MIGRATION_FILENAME).exists() {
        return Err(EngineError::EncryptionMigrationIncomplete {
            detail: format!(
                "found `{ENCRYPTION_MIGRATION_FILENAME}` — a prior `Engine::reencrypt` did not \
                 finish. Restore from backup, or remove the marker manually if you can prove \
                 all files use the same key."
            ),
        });
    }
    let marker = read_encryption_marker(db_dir)?;
    match (hint, marker) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(EngineError::EncryptionKeyMismatch {
            detail: "database is encrypted but no key supplied; pass the cipher to \
                     Engine::open_with_cipher (or set NDB_ENC_KEY + use open_from_env)"
                .into(),
        }),
        (Some(_), None) => Err(EngineError::EncryptionKeyMismatch {
            detail: "key supplied but database has no encryption marker; \
                     use Engine::reencrypt to migrate, or drop the key to open plaintext"
                .into(),
        }),
        (Some(c), Some(m)) => {
            if m.matches(&c) {
                Ok(Some(c))
            } else {
                Err(EngineError::EncryptionKeyMismatch {
                    detail: "supplied key does not match the database's stored fingerprint".into(),
                })
            }
        }
    }
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
    fn engine_config_default_is_back_compat() {
        // open == open_with_config(default); default reproduces historical
        // behaviour (RAM rebuild, no mmap indexes).
        let cfg = EngineConfig::default();
        assert_eq!(cfg.max_cache_bytes, DEFAULT_MAX_CACHE_BYTES);
        assert!(!cfg.mmap_indexes);
        assert!(!cfg.low_memory);
        // resolved() leaves a plain default untouched.
        assert_eq!(cfg.resolved(), cfg);
    }

    #[test]
    fn low_memory_preset_resolves_mmap_indexes() {
        let cfg = EngineConfig {
            low_memory: true,
            ..EngineConfig::default()
        };
        // low_memory implies mmap_indexes once resolved.
        assert!(!cfg.mmap_indexes);
        assert!(cfg.resolved().mmap_indexes);
        // helper sets both directly.
        let lm = EngineConfig::low_memory(512 * 1024 * 1024);
        assert!(lm.mmap_indexes);
        assert!(lm.low_memory);
        assert_eq!(lm.max_cache_bytes, 512 * 1024 * 1024);
    }

    #[test]
    fn open_with_config_round_trips_config() {
        let dir = temp_dir("open_with_config");
        Engine::create(&dir).unwrap().close().unwrap();
        let cfg = EngineConfig::low_memory(123 * 1024 * 1024);
        let engine = Engine::open_with_config(&dir, cfg).unwrap();
        assert_eq!(engine.config(), cfg.resolved());
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn index_memory_stats_grows_with_data() {
        let dir = temp_dir("index_mem_stats");
        let mut engine = Engine::create(&dir).unwrap();
        let cust = TypeId::new(1);
        let age = PropertyId::new(1);
        engine.register_property_btree(cust, age);
        let empty = engine.index_memory_stats();
        // Only the registration entry costs anything before data lands.
        assert!(empty.entity_type_cluster == 0);
        for i in 0..500u64 {
            let mut txn = engine.begin_write();
            txn.put_entity(EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: cust,
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![(age, Value::I64(i64::try_from(i).unwrap()))],
            });
            txn.commit().unwrap();
        }
        let stats = engine.index_memory_stats();
        // Entity-type cluster + property-btree both populated.
        assert!(stats.entity_type_cluster > 0);
        assert!(stats.property_btree > 0);
        assert!(stats.index_total() > empty.index_total());
        assert!(stats.total() >= stats.index_total());
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flush_writes_property_index_sidecar() {
        use crate::index::property_index_file::{PropertyIndexFile, sidecar_path_for};
        let dir = temp_dir("pidx_flush");
        let cust = TypeId::new(1);
        let age = PropertyId::new(1);
        let a = EntityId::now_v7();
        let b = EntityId::now_v7();
        let mut engine = Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
        engine.register_property_btree(cust, age);
        for (eid, v) in [(a, 30i64), (b, 40)] {
            let mut txn = engine.begin_write();
            txn.put_entity(EntityRecord {
                entity_id: eid,
                type_id: cust,
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![(age, Value::I64(v))],
            });
            txn.commit().unwrap();
        }
        engine.flush().unwrap();
        // Exactly one SSTable → one .pidx sidecar.
        let sst_seq = engine.manifest().sstables[0].file_seq;
        let sst_path = super::sstable_path(engine.path(), sst_seq);
        let pidx = sidecar_path_for(&sst_path);
        assert!(pidx.exists(), "expected .pidx sidecar at {pidx:?}");
        // It reflects the indexed values.
        let f = PropertyIndexFile::open(&pidx).unwrap().unwrap();
        let bytes30 = super::value_to_index_bytes(&Value::I64(30)).unwrap();
        assert_eq!(f.find(cust, age, &bytes30), vec![a]);
        let top = f.top_k(cust, age, 1);
        assert_eq!(top[0].1, b); // 40 is highest
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_sidecar_when_no_registration() {
        use crate::index::property_index_file::sidecar_path_for;
        let dir = temp_dir("pidx_none");
        let mut engine = Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
        let mut txn = engine.begin_write();
        txn.put_entity(make_entity(EntityId::now_v7(), "x"));
        txn.commit().unwrap();
        engine.flush().unwrap();
        let sst_seq = engine.manifest().sstables[0].file_seq;
        let pidx = sidecar_path_for(&super::sstable_path(engine.path(), sst_seq));
        assert!(!pidx.exists(), "no registration → no sidecar");
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // --- Phase 1c: property index served from disk (low-RAM mode) -------

    fn put_cites(engine: &mut Engine, eid: EntityId, ty: TypeId, prop: PropertyId, v: i64) {
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: eid,
            type_id: ty,
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(prop, Value::I64(v))],
        });
        txn.commit().unwrap();
    }

    /// Reopen + re-register + rebuild (the langgraph-server usage pattern).
    fn reopen_registered(dir: &PathBuf, ty: TypeId, prop: PropertyId, cfg: EngineConfig) -> Engine {
        let mut e = Engine::open_with_config(dir, cfg).unwrap();
        e.register_property_btree(ty, prop);
        e.rebuild_indexes().unwrap();
        e
    }

    #[test]
    fn low_memory_query_matches_default() {
        let dir = temp_dir("lm_match");
        let ty = TypeId::new(1);
        let prop = PropertyId::new(2);
        {
            let mut e = Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            e.register_property_btree(ty, prop);
            for v in [10i64, 250, 30, 999, 7, 250, 88] {
                put_cites(&mut e, EntityId::now_v7(), ty, prop, v);
                e.flush().unwrap(); // one SSTable + sidecar per value
            }
            e.close().unwrap();
        }
        // Single-process LOCK → open one engine at a time. Collect default
        // results, close, then compare against low-memory.
        let (mut d_find, mut d_range, d_top) = {
            let def = reopen_registered(&dir, ty, prop, EngineConfig::default());
            let f = def.property_lookup(ty, prop, &Value::I64(250));
            let r = def.property_range(ty, prop, Some(&Value::I64(30)), Some(&Value::I64(300)));
            let t = def.property_top_k(ty, prop, 3);
            def.close().unwrap();
            (f, r, t)
        };
        d_find.sort();
        d_range.sort();

        let lm = reopen_registered(&dir, ty, prop, EngineConfig::low_memory(64 * 1024 * 1024));
        assert!(lm.config().mmap_indexes);
        let mut l_find = lm.property_lookup(ty, prop, &Value::I64(250));
        l_find.sort();
        assert_eq!(d_find, l_find);
        assert_eq!(l_find.len(), 2);
        let mut l_range = lm.property_range(ty, prop, Some(&Value::I64(30)), Some(&Value::I64(300)));
        l_range.sort();
        assert_eq!(d_range, l_range);
        assert_eq!(d_top, lm.property_top_k(ty, prop, 3));
        lm.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn low_memory_mvcc_update_and_tombstone() {
        let dir = temp_dir("lm_mvcc");
        let ty = TypeId::new(1);
        let prop = PropertyId::new(2);
        let updated = EntityId::now_v7();
        let deleted = EntityId::now_v7();
        let stable = EntityId::now_v7();
        {
            let mut e = Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            e.register_property_btree(ty, prop);
            put_cites(&mut e, updated, ty, prop, 10);
            put_cites(&mut e, deleted, ty, prop, 20);
            put_cites(&mut e, stable, ty, prop, 30);
            e.flush().unwrap();
            // Update `updated` to 999, tombstone `deleted`; flush again.
            put_cites(&mut e, updated, ty, prop, 999);
            let mut txn = e.begin_write();
            txn.delete(deleted.into_uuid());
            txn.commit().unwrap();
            e.flush().unwrap();
            e.close().unwrap();
        }
        let lm = reopen_registered(&dir, ty, prop, EngineConfig::low_memory(64 * 1024 * 1024));
        // Stale value gone, current value present.
        assert!(lm.property_lookup(ty, prop, &Value::I64(10)).is_empty());
        assert_eq!(lm.property_lookup(ty, prop, &Value::I64(999)), vec![updated]);
        // Tombstoned entity excluded everywhere.
        assert!(lm.property_lookup(ty, prop, &Value::I64(20)).is_empty());
        assert_eq!(lm.property_lookup(ty, prop, &Value::I64(30)), vec![stable]);
        // top_k by CURRENT value: 999 (updated), 30 (stable); deleted gone.
        assert_eq!(lm.property_top_k(ty, prop, 5), vec![updated, stable]);
        // Range honours current values too.
        assert_eq!(
            lm.property_range(ty, prop, Some(&Value::I64(500)), None),
            vec![updated]
        );
        lm.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn low_memory_property_ram_bounded_after_flushes() {
        let dir = temp_dir("lm_bounded");
        let ty = TypeId::new(1);
        let prop = PropertyId::new(2);
        {
            let mut e = Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            e.register_property_btree(ty, prop);
            for i in 0..2000i64 {
                put_cites(&mut e, EntityId::now_v7(), ty, prop, i);
                if i % 200 == 199 {
                    e.flush().unwrap();
                }
            }
            e.flush().unwrap();
            e.close().unwrap();
        }
        // One engine at a time (single-process LOCK).
        let (dpb, d_top1, d_find) = {
            let def = reopen_registered(&dir, ty, prop, EngineConfig::default());
            let r = (
                def.index_memory_stats().property_btree,
                def.property_top_k(ty, prop, 1),
                def.property_lookup(ty, prop, &Value::I64(1500)),
            );
            def.close().unwrap();
            r
        };
        let lm = reopen_registered(&dir, ty, prop, EngineConfig::low_memory(64 * 1024 * 1024));
        let lpb = lm.index_memory_stats().property_btree;
        assert!(dpb > 10_000, "default holds full property index, got {dpb}");
        assert!(
            lpb * 4 < dpb,
            "low-memory RAM property mirror should be far smaller: lm={lpb} def={dpb}"
        );
        assert_eq!(lm.property_top_k(ty, prop, 1), d_top1);
        assert_eq!(lm.property_lookup(ty, prop, &Value::I64(1500)), d_find);
        lm.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn low_memory_compaction_keeps_sidecar_and_correctness() {
        let dir = temp_dir("lm_compact");
        let ty = TypeId::new(1);
        let prop = PropertyId::new(2);
        let top = EntityId::now_v7();
        {
            let mut e = Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            e.register_property_btree(ty, prop);
            put_cites(&mut e, top, ty, prop, 5000);
            e.flush().unwrap();
            for i in 0..50i64 {
                put_cites(&mut e, EntityId::now_v7(), ty, prop, i);
                e.flush().unwrap();
            }
            e.compact().unwrap(); // merges all → single SSTable + one .pidx
            e.close().unwrap();
        }
        let lm = reopen_registered(&dir, ty, prop, EngineConfig::low_memory(64 * 1024 * 1024));
        assert_eq!(lm.property_top_k(ty, prop, 1), vec![top]);
        assert_eq!(lm.property_lookup(ty, prop, &Value::I64(5000)), vec![top]);
        lm.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // --- Phase 2: vector index served from disk (low-RAM mode) ----------

    fn put_vec(engine: &mut Engine, eid: EntityId, prop: PropertyId, v: Vec<f32>) {
        let mut txn = engine.begin_write();
        txn.put_entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(prop, Value::Vector(v))],
        });
        txn.commit().unwrap();
    }

    fn reopen_vec(dir: &PathBuf, prop: PropertyId, cfg: EngineConfig) -> Engine {
        let mut e = Engine::open_with_config(dir, cfg).unwrap();
        e.register_vector_property(prop);
        e.rebuild_indexes().unwrap();
        e
    }

    #[test]
    fn low_memory_vector_search_matches_default() {
        let dir = temp_dir("lm_vec_match");
        let prop = PropertyId::new(3);
        let mut ids = Vec::new();
        {
            let mut e = Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            e.register_vector_property(prop);
            for i in 0..8u32 {
                let id = EntityId::now_v7();
                ids.push(id);
                put_vec(&mut e, id, prop, vec![f64::from(i) as f32, 0.0, 1.0]);
                e.flush().unwrap(); // one SSTable + .vidx per row
            }
            e.close().unwrap();
        }
        let q = [3.2f32, 0.0, 1.0];
        let d_ids = {
            let e = reopen_vec(&dir, prop, EngineConfig::default());
            let r: Vec<EntityId> = e
                .vector_search(prop, &q, 3, Distance::L2Squared)
                .into_iter()
                .map(|(x, _)| x)
                .collect();
            e.close().unwrap();
            r
        };
        let lm = reopen_vec(&dir, prop, EngineConfig::low_memory(64 * 1024 * 1024));
        assert!(lm.config().mmap_indexes);
        let l_ids: Vec<EntityId> = lm
            .vector_search(prop, &q, 3, Distance::L2Squared)
            .into_iter()
            .map(|(x, _)| x)
            .collect();
        assert_eq!(d_ids, l_ids);
        assert_eq!(l_ids[0], ids[3]); // 3.0 is closest to 3.2
        lm.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vector_snapshot_matches_exact_and_persists() {
        let dir = temp_dir("vec_snapshot");
        let prop = PropertyId::new(3);
        let mut ids = Vec::new();
        {
            let mut e =
                Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            e.register_vector_property(prop);
            for i in 0..8u32 {
                let id = EntityId::now_v7();
                ids.push(id);
                put_vec(&mut e, id, prop, vec![f64::from(i) as f32, 0.0, 1.0]);
                e.flush().unwrap(); // 8 SSTables → 8 .vidx sidecars (fan-out scenario)
            }
            e.close().unwrap();
        }
        let q = [3.2f32, 0.0, 1.0];
        let mut e = reopen_vec(&dir, prop, EngineConfig::low_memory(64 * 1024 * 1024));
        // Multi-sidecar exact path (the baseline the snapshot must reproduce).
        let exact: Vec<EntityId> = e
            .vector_search(prop, &q, 3, Distance::L2Squared)
            .into_iter()
            .map(|(x, _)| x)
            .collect();
        // Build the global snapshot, search it — same nearest, in order.
        assert_eq!(e.build_vector_snapshot(prop).unwrap(), 8);
        assert!(e.has_vector_snapshot(prop));
        let snap: Vec<EntityId> = e
            .vector_search_snapshot(prop, &q, 3, Distance::L2Squared)
            .expect("snapshot present")
            .into_iter()
            .map(|(x, _)| x)
            .collect();
        assert_eq!(snap, exact, "snapshot kNN must match exact multi-sidecar kNN");
        assert_eq!(snap[0], ids[3]); // 3.0 closest to 3.2
        e.close().unwrap();
        // Persists: a fresh open does NOT auto-load, but load_vector_snapshot
        // re-mmaps the on-disk .vsnap and search works without a rebuild.
        let mut e2 = reopen_vec(&dir, prop, EngineConfig::low_memory(64 * 1024 * 1024));
        assert!(!e2.has_vector_snapshot(prop));
        assert!(e2.load_vector_snapshot(prop).unwrap());
        let snap2: Vec<EntityId> = e2
            .vector_search_snapshot(prop, &q, 3, Distance::L2Squared)
            .unwrap()
            .into_iter()
            .map(|(x, _)| x)
            .collect();
        assert_eq!(snap2, exact);
        e2.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vector_snapshot_reflects_current_after_update_and_tombstone() {
        // The snapshot streams CURRENT vectors, so it needs no per-candidate
        // MVCC verify: a moved entity ranks by its new value, a deleted one
        // is absent — proven by building the snapshot AFTER both mutations.
        let dir = temp_dir("vec_snapshot_mvcc");
        let prop = PropertyId::new(3);
        let moved = EntityId::now_v7();
        let deleted = EntityId::now_v7();
        let stable = EntityId::now_v7();
        {
            let mut e =
                Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            e.register_vector_property(prop);
            put_vec(&mut e, moved, prop, vec![9.0, 9.0]); // initially far
            put_vec(&mut e, deleted, prop, vec![0.0, 0.0]); // initially nearest
            put_vec(&mut e, stable, prop, vec![1.0, 1.0]);
            e.flush().unwrap();
            put_vec(&mut e, moved, prop, vec![0.05, 0.0]); // now nearest
            let mut txn = e.begin_write();
            txn.delete(deleted.into_uuid());
            txn.commit().unwrap();
            e.flush().unwrap();
            e.close().unwrap();
        }
        let mut e = reopen_vec(&dir, prop, EngineConfig::low_memory(64 * 1024 * 1024));
        assert_eq!(e.build_vector_snapshot(prop).unwrap(), 2); // moved + stable, NOT deleted
        let q = [0.0f32, 0.0];
        let hits: Vec<EntityId> = e
            .vector_search_snapshot(prop, &q, 3, Distance::L2Squared)
            .unwrap()
            .into_iter()
            .map(|(x, _)| x)
            .collect();
        assert_eq!(hits.len(), 2, "deleted entity must be absent");
        assert_eq!(hits[0], moved, "moved entity ranks by its CURRENT (near) vector");
        assert!(!hits.contains(&deleted));
        e.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn low_memory_vector_mvcc_update_and_tombstone() {
        let dir = temp_dir("lm_vec_mvcc");
        let prop = PropertyId::new(3);
        let moved = EntityId::now_v7();
        let deleted = EntityId::now_v7();
        let stable = EntityId::now_v7();
        {
            let mut e = Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            e.register_vector_property(prop);
            put_vec(&mut e, moved, prop, vec![9.0, 9.0]); // initially far
            put_vec(&mut e, deleted, prop, vec![0.0, 0.0]); // initially nearest
            put_vec(&mut e, stable, prop, vec![1.0, 1.0]);
            e.flush().unwrap();
            // Move `moved` next to the query; tombstone `deleted`; flush.
            put_vec(&mut e, moved, prop, vec![0.05, 0.0]);
            let mut txn = e.begin_write();
            txn.delete(deleted.into_uuid());
            txn.commit().unwrap();
            e.flush().unwrap();
            e.close().unwrap();
        }
        let lm = reopen_vec(&dir, prop, EngineConfig::low_memory(64 * 1024 * 1024));
        let q = [0.0f32, 0.0];
        let ids: Vec<EntityId> = lm
            .vector_search(prop, &q, 5, Distance::L2Squared)
            .into_iter()
            .map(|(x, _)| x)
            .collect();
        assert!(!ids.contains(&deleted), "tombstoned entity must be excluded");
        assert_eq!(ids[0], moved, "updated embedding should now rank first");
        assert!(ids.contains(&stable));
        lm.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn low_memory_vector_ram_bounded_after_flushes() {
        let dir = temp_dir("lm_vec_bounded");
        let prop = PropertyId::new(3);
        {
            let mut e = Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            e.register_vector_property(prop);
            for i in 0..2000u32 {
                put_vec(
                    &mut e,
                    EntityId::now_v7(),
                    prop,
                    vec![f64::from(i) as f32, f64::from(i % 13) as f32, 1.0],
                );
                if i % 200 == 199 {
                    e.flush().unwrap();
                }
            }
            e.flush().unwrap();
            e.close().unwrap();
        }
        let dvec = {
            let e = reopen_vec(&dir, prop, EngineConfig::default());
            let r = e.index_memory_stats().vector;
            e.close().unwrap();
            r
        };
        let lm = reopen_vec(&dir, prop, EngineConfig::low_memory(64 * 1024 * 1024));
        let lvec = lm.index_memory_stats().vector;
        assert!(dvec > 50_000, "default holds all embeddings, got {dvec}");
        assert!(
            lvec * 4 < dvec,
            "low-memory vector RAM should be far smaller: lm={lvec} def={dvec}"
        );
        lm.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // --- Phase 2e: id-list indexes served from disk (low-RAM mode) ------

    const T_PAPER: u32 = 1;
    const T_AUTHOR: u32 = 2;
    const T_CITES: u32 = 100;
    const P_NAME: u32 = 1;

    fn put_named(e: &mut Engine, id: EntityId, ty: u32, nm: &str) {
        let mut tx = e.begin_write();
        tx.put_entity(EntityRecord {
            entity_id: id,
            type_id: TypeId::new(ty),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(P_NAME), Value::String(nm.into()))],
        });
        tx.commit().unwrap();
    }

    fn put_cites_edge(e: &mut Engine, hid: HyperedgeId, src: EntityId, dst: EntityId) {
        let mut tx = e.begin_write();
        tx.put_hyperedge(HyperEdgeRecord {
            hyperedge_id: hid,
            type_id: TypeId::new(T_CITES),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(1), src), (RoleId::new(2), dst)],
            hyperedge_roles: vec![],
            properties: vec![],
        });
        tx.commit().unwrap();
    }

    fn reopen_full(dir: &PathBuf, cfg: EngineConfig) -> Engine {
        let mut e = Engine::open_with_config(dir, cfg).unwrap();
        e.register_lookup_key(PropertyId::new(P_NAME));
        e.rebuild_indexes().unwrap();
        e
    }

    fn build_graph(dir: &PathBuf) -> (Vec<EntityId>, EntityId, Vec<HyperedgeId>) {
        let mut papers = Vec::new();
        let mut edges = Vec::new();
        let mut e =
            Engine::create_with_config(dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
        e.register_lookup_key(PropertyId::new(P_NAME));
        for i in 0..6u32 {
            let id = EntityId::now_v7();
            papers.push(id);
            put_named(&mut e, id, T_PAPER, &format!("paper-{i}"));
            e.flush().unwrap();
        }
        let author = EntityId::now_v7();
        put_named(&mut e, author, T_AUTHOR, "alice");
        e.flush().unwrap();
        for i in 1..6usize {
            let hid = HyperedgeId::now_v7();
            edges.push(hid);
            put_cites_edge(&mut e, hid, papers[i], papers[i - 1]);
            e.flush().unwrap();
        }
        e.close().unwrap();
        (papers, author, edges)
    }

    #[test]
    fn low_memory_id_list_indexes_match_default() {
        let dir = temp_dir("lm_idlist");
        let (papers, author, edges) = build_graph(&dir);
        let paper = TypeId::new(T_PAPER);
        let author_ty = TypeId::new(T_AUTHOR);
        let cites = TypeId::new(T_CITES);
        let name = PropertyId::new(P_NAME);

        let reference = {
            let e = reopen_full(&dir, EngineConfig::default());
            let mut nb = e.hyperedges_for_entity(papers[2]);
            nb.sort();
            let mut by_t = e.hyperedges_by_type(cites);
            by_t.sort();
            let mut ents = e.entities_by_type(paper);
            ents.sort();
            let r = (
                nb,
                by_t,
                ents,
                e.entities_by_type(author_ty),
                e.lookup_by_external_key(name, &Value::String("paper-3".into())),
                e.entity_type_count(paper),
                e.hyperedge_type_count(cites),
            );
            e.close().unwrap();
            r
        };

        let lm = reopen_full(&dir, EngineConfig::low_memory(64 * 1024 * 1024));
        assert!(lm.adjacency_served_from_disk());
        assert!(lm.type_cluster_served_from_disk());
        assert!(lm.entity_type_served_from_disk());
        assert!(lm.lookup_served_from_disk());

        let mut nb = lm.hyperedges_for_entity(papers[2]);
        nb.sort();
        assert_eq!(nb, reference.0);
        assert_eq!(nb.len(), 2); // papers[2] in edge[2] (src) + edge[3] (dst)
        let mut by_t = lm.hyperedges_by_type(cites);
        by_t.sort();
        assert_eq!(by_t, reference.1);
        assert_eq!(by_t.len(), 5);
        let mut ents = lm.entities_by_type(paper);
        ents.sort();
        assert_eq!(ents, reference.2);
        assert_eq!(ents.len(), 6);
        assert_eq!(lm.entities_by_type(author_ty), reference.3);
        assert_eq!(lm.entities_by_type(author_ty), vec![author]);
        assert_eq!(
            lm.lookup_by_external_key(name, &Value::String("paper-3".into())),
            reference.4
        );
        assert_eq!(
            lm.lookup_by_external_key(name, &Value::String("paper-3".into())),
            Some(papers[3])
        );
        assert_eq!(lm.entity_type_count(paper), reference.5);
        assert_eq!(lm.entity_type_count(paper), 6);
        assert_eq!(lm.hyperedge_type_count(cites), reference.6);
        assert_eq!(lm.hyperedge_type_count(cites), 5);
        assert!(edges.contains(&by_t[0]));
        lm.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn low_memory_id_list_mvcc_tombstone() {
        let dir = temp_dir("lm_idlist_mvcc");
        let paper = TypeId::new(T_PAPER);
        let cites = TypeId::new(T_CITES);
        let name = PropertyId::new(P_NAME);
        let p0 = EntityId::now_v7();
        let p1 = EntityId::now_v7();
        let p2 = EntityId::now_v7();
        let keep_edge = HyperedgeId::now_v7();
        let drop_edge = HyperedgeId::now_v7();
        {
            let mut e =
                Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            e.register_lookup_key(name);
            put_named(&mut e, p0, T_PAPER, "p0");
            put_named(&mut e, p1, T_PAPER, "p1");
            put_named(&mut e, p2, T_PAPER, "p2");
            put_cites_edge(&mut e, keep_edge, p1, p0);
            put_cites_edge(&mut e, drop_edge, p2, p1);
            e.flush().unwrap();
            let mut tx = e.begin_write();
            tx.delete(drop_edge.into_uuid());
            tx.commit().unwrap();
            let mut tx = e.begin_write();
            tx.delete(p2.into_uuid());
            tx.commit().unwrap();
            e.flush().unwrap();
            e.close().unwrap();
        }
        let lm = reopen_full(&dir, EngineConfig::low_memory(64 * 1024 * 1024));
        assert_eq!(lm.hyperedges_by_type(cites), vec![keep_edge]);
        assert_eq!(lm.hyperedge_type_count(cites), 1);
        assert!(!lm.hyperedge_has_type(drop_edge, cites));
        assert!(lm.hyperedge_has_type(keep_edge, cites));
        assert_eq!(lm.hyperedges_for_entity(p1), vec![keep_edge]);
        let mut papers = lm.entities_by_type(paper);
        papers.sort();
        let mut want = vec![p0, p1];
        want.sort();
        assert_eq!(papers, want);
        assert_eq!(lm.entity_type_count(paper), 2);
        assert!(lm.lookup_by_external_key(name, &Value::String("p2".into())).is_none());
        assert_eq!(lm.lookup_by_external_key(name, &Value::String("p0".into())), Some(p0));
        lm.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn low_memory_open_preserves_meta_via_sidecar() {
        // The needs_scan skip means a low-RAM open does NOT scan SSTables
        // whose indexes are all on disk — so tx timestamps + retention must
        // survive via the .meta sidecar instead.
        let dir = temp_dir("lm_meta");
        let ty = TypeId::new(1);
        let mut tx_ids = Vec::new();
        {
            let mut e =
                Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            e.set_retention_policy(ty, RetentionPolicy::Audited);
            for i in 0..3 {
                let mut tx = e.begin_write();
                tx.put_entity(make_entity(EntityId::now_v7(), &format!("x{i}")));
                tx_ids.push(tx.commit().unwrap());
            }
            e.flush().unwrap();
            e.close().unwrap();
        }
        // Reopen low-RAM: timestamps + retention restored from .meta.
        let e = Engine::open_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
        for tid in &tx_ids {
            assert!(
                e.commit_timestamp_us(*tid).is_some(),
                "tx {tid:?} timestamp lost after low-mem reopen"
            );
        }
        assert!(
            matches!(e.retention_policy(ty), RetentionPolicy::Audited),
            "retention lost after low-mem reopen"
        );
        e.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn low_memory_id_list_ram_bounded() {
        let dir = temp_dir("lm_idlist_bounded");
        {
            let mut e =
                Engine::create_with_config(&dir, EngineConfig::low_memory(64 * 1024 * 1024)).unwrap();
            let mut prev: Option<EntityId> = None;
            for i in 0..2000u32 {
                let id = EntityId::now_v7();
                put_named(&mut e, id, T_PAPER, &format!("p{i}"));
                if let Some(pv) = prev {
                    put_cites_edge(&mut e, HyperedgeId::now_v7(), id, pv);
                }
                prev = Some(id);
                if i % 200 == 199 {
                    e.flush().unwrap();
                }
            }
            e.flush().unwrap();
            e.close().unwrap();
        }
        let dadj = {
            let e = Engine::open(&dir).unwrap();
            let r = e.index_memory_stats().adjacency;
            e.close().unwrap();
            r
        };
        let lm = reopen_full(&dir, EngineConfig::low_memory(64 * 1024 * 1024));
        let ladj = lm.index_memory_stats().adjacency;
        assert!(dadj > 20_000, "default holds full adjacency, got {dadj}");
        assert!(
            ladj * 4 < dadj,
            "low-memory adjacency RAM should be far smaller: lm={ladj} def={dadj}"
        );
        lm.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn hot_backup_captures_flushed_and_unflushed_state() {
        let src = temp_dir("backup_src");
        let dst = temp_dir("backup_dst");
        let (a, b, c) = (EntityId::now_v7(), EntityId::now_v7(), EntityId::now_v7());
        let (ta, tb, tc);
        {
            let mut engine = Engine::create(&src).unwrap();
            // A: committed, left in the WAL/memtable (not flushed).
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(a, "alice"));
            ta = txn.commit().unwrap();
            // B: committed then flushed to an SSTable.
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(b, "bob"));
            tb = txn.commit().unwrap();
            engine.flush().unwrap();
            assert_eq!(engine.sstable_count(), 1);
            // C: committed AFTER the flush, into the fresh rotated WAL.
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(c, "carol"));
            tc = txn.commit().unwrap();

            // Hot backup while the engine is still open + serving.
            let stats = engine.backup_to(&dst).unwrap();
            assert!(stats.sstables >= 1, "backup must copy the SSTable");
            assert!(stats.files_copied >= 3, "CURRENT + MANIFEST + data at least");
            engine.close().unwrap();
        }

        // Open the BACKUP directory as a fresh database — all three entities
        // must be visible: B from the copied SSTable, A and C from the
        // replayed copied WAL.
        let restored = Engine::open(&dst).unwrap();
        for (eid, tx, who) in [(a, ta, "alice"), (b, tb, "bob"), (c, tc, "carol")] {
            match restored.snapshot_read(&eid.into_uuid(), tx).unwrap() {
                Resolved::Live(Record::Entity(e)) => assert_eq!(e.entity_id, eid, "{who}"),
                other => panic!("restored read of {who}: {other:?}"),
            }
        }
        restored.close().unwrap();
        std::fs::remove_dir_all(&src).unwrap();
        std::fs::remove_dir_all(&dst).unwrap();
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
                hyperedge_roles: Vec::new(),
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
                hyperedge_roles: Vec::new(),
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
                    hyperedge_roles: Vec::new(),
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
                    hyperedge_roles: Vec::new(),
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

    // ---------------------------------------------------------------------
    // Encryption marker behaviour. Tests pass the cipher explicitly via
    // `create_with_cipher` / `open_with_cipher` to avoid racing against
    // NDB_ENC_KEY in parallel runs.
    // ---------------------------------------------------------------------

    fn cipher_a() -> Cipher {
        Cipher::from_raw_key(&[0x11u8; 32]).unwrap()
    }
    fn cipher_b() -> Cipher {
        Cipher::from_raw_key(&[0x22u8; 32]).unwrap()
    }

    #[test]
    fn encrypted_engine_create_writes_marker_and_round_trips() {
        let dir = temp_dir("enc_engine_create");
        let mut engine = Engine::create_with_cipher(&dir, Some(cipher_a())).unwrap();
        let eid = EntityId::now_v7();
        let mut txn = engine.begin_write();
        txn.put_entity(make_entity(eid, "alice"));
        txn.commit().unwrap();
        engine.flush().unwrap();
        engine.close().unwrap();

        // Marker file present; the WAL + SSTable encryption is exercised
        // by lower-layer tests; here we focus on the cross-restart flow.
        let marker_path = dir.join(crate::encryption::ENCRYPTION_MARKER_FILENAME);
        assert!(marker_path.exists(), "marker file should exist");

        // Reopen with the same key → entity visible.
        let mut engine = Engine::open_with_cipher(&dir, Some(cipher_a())).unwrap();
        let snap = TxId::new(engine.manifest().last_tx_id);
        let resolved = engine.snapshot_read(&eid.into_uuid(), snap).unwrap();
        assert!(matches!(resolved, Resolved::Live(_)));
        engine.close().unwrap();

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn encrypted_engine_wrong_key_refused() {
        let dir = temp_dir("enc_engine_wrong_key");
        let mut engine = Engine::create_with_cipher(&dir, Some(cipher_a())).unwrap();
        let mut txn = engine.begin_write();
        txn.put_entity(make_entity(EntityId::now_v7(), "alice"));
        txn.commit().unwrap();
        engine.close().unwrap();

        let err = Engine::open_with_cipher(&dir, Some(cipher_b())).unwrap_err();
        assert!(
            matches!(err, EngineError::EncryptionKeyMismatch { .. }),
            "wrong key must produce EncryptionKeyMismatch, got: {err:?}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn encrypted_engine_missing_key_on_open_refused() {
        let dir = temp_dir("enc_engine_no_key");
        Engine::create_with_cipher(&dir, Some(cipher_a()))
            .unwrap()
            .close()
            .unwrap();

        let err = Engine::open_with_cipher(&dir, None).unwrap_err();
        assert!(
            matches!(err, EngineError::EncryptionKeyMismatch { .. }),
            "encrypted DB without key must be refused, got: {err:?}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn plaintext_engine_with_unexpected_key_refused() {
        let dir = temp_dir("enc_engine_unexpected_key");
        // Create a plaintext database.
        let mut engine = Engine::create(&dir).unwrap();
        let mut txn = engine.begin_write();
        txn.put_entity(make_entity(EntityId::now_v7(), "alice"));
        txn.commit().unwrap();
        engine.close().unwrap();

        // Open with a key — must refuse (no implicit migration).
        let err = Engine::open_with_cipher(&dir, Some(cipher_a())).unwrap_err();
        assert!(
            matches!(err, EngineError::EncryptionKeyMismatch { .. }),
            "plaintext DB + key must be refused, got: {err:?}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn encrypted_engine_restart_replays_wal_and_flushes_encrypted_sstable() {
        let dir = temp_dir("enc_engine_full_lifecycle");
        // Commit some records BEFORE flush — they live only in the WAL.
        // Restart must replay them out of the encrypted WAL.
        let mut engine = Engine::create_with_cipher(&dir, Some(cipher_a())).unwrap();
        let eid = EntityId::now_v7();
        let mut txn = engine.begin_write();
        txn.put_entity(make_entity(eid, "wal-only"));
        txn.commit().unwrap();
        engine.close().unwrap();

        let mut engine = Engine::open_with_cipher(&dir, Some(cipher_a())).unwrap();
        let snap = TxId::new(engine.manifest().last_tx_id);
        let resolved = engine.snapshot_read(&eid.into_uuid(), snap).unwrap();
        assert!(matches!(resolved, Resolved::Live(_)));
        // Flush to encrypted SSTable, restart again, still visible.
        engine.flush().unwrap();
        engine.close().unwrap();

        let mut engine = Engine::open_with_cipher(&dir, Some(cipher_a())).unwrap();
        let snap = TxId::new(engine.manifest().last_tx_id);
        let resolved = engine.snapshot_read(&eid.into_uuid(), snap).unwrap();
        assert!(matches!(resolved, Resolved::Live(_)));
        engine.close().unwrap();

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---------------------------------------------------------------------
    // §2.1 Engine::reencrypt — key rotation + plaintext↔encrypted migration
    // ---------------------------------------------------------------------

    fn seed_n_entities(engine: &mut Engine, n: usize) -> Vec<EntityId> {
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let eid = EntityId::now_v7();
            let mut txn = engine.begin_write();
            txn.put_entity(EntityRecord {
                entity_id: eid,
                type_id: TypeId::new(42),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![(PropertyId::new(1), Value::I64(i64::try_from(i).unwrap_or(0)))],
            });
            txn.commit().unwrap();
            ids.push(eid);
        }
        ids
    }

    fn assert_visible(engine: &mut Engine, ids: &[EntityId]) {
        let snap = TxId::new(engine.manifest().last_tx_id);
        for eid in ids {
            let resolved = engine.snapshot_read(&eid.into_uuid(), snap).unwrap();
            assert!(
                matches!(resolved, Resolved::Live(_)),
                "entity {eid:?} not visible after reencrypt"
            );
        }
    }

    #[test]
    fn reencrypt_plaintext_to_encrypted_round_trip() {
        let dir = temp_dir("reencrypt_plain_to_enc");
        let mut engine = Engine::create(&dir).unwrap();
        let ids = seed_n_entities(&mut engine, 5);
        engine.flush().unwrap();

        let stats = engine.reencrypt(Some(&cipher_a())).unwrap();
        assert!(stats.sstables_rewritten >= 1);
        assert_eq!(stats.wal_segments_rewritten, 1);
        assert!(stats.bytes_rewritten > 0);

        // Marker now present.
        assert!(dir.join(crate::encryption::ENCRYPTION_MARKER_FILENAME).exists());
        // Transient marker gone.
        assert!(!dir.join(crate::encryption::ENCRYPTION_MIGRATION_FILENAME).exists());

        // Records still visible from the still-open Engine.
        assert_visible(&mut engine, &ids);
        engine.close().unwrap();

        // Reopen with the new key — records still visible.
        let mut engine = Engine::open_with_cipher(&dir, Some(cipher_a())).unwrap();
        assert_visible(&mut engine, &ids);
        // Reopen without a key → refused.
        engine.close().unwrap();
        let err = Engine::open(&dir).unwrap_err();
        assert!(matches!(err, EngineError::EncryptionKeyMismatch { .. }));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reencrypt_encrypted_to_encrypted_new_key() {
        let dir = temp_dir("reencrypt_rotate");
        let mut engine = Engine::create_with_cipher(&dir, Some(cipher_a())).unwrap();
        let ids = seed_n_entities(&mut engine, 5);
        engine.flush().unwrap();

        let stats = engine.reencrypt(Some(&cipher_b())).unwrap();
        assert_eq!(stats.wal_segments_rewritten, 1);
        assert_visible(&mut engine, &ids);
        engine.close().unwrap();

        // Old key now refused; new key works.
        let err = Engine::open_with_cipher(&dir, Some(cipher_a())).unwrap_err();
        assert!(matches!(err, EngineError::EncryptionKeyMismatch { .. }));
        let mut engine = Engine::open_with_cipher(&dir, Some(cipher_b())).unwrap();
        assert_visible(&mut engine, &ids);
        engine.close().unwrap();

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reencrypt_encrypted_to_plaintext() {
        let dir = temp_dir("reencrypt_strip");
        let mut engine = Engine::create_with_cipher(&dir, Some(cipher_a())).unwrap();
        let ids = seed_n_entities(&mut engine, 3);
        engine.flush().unwrap();

        let stats = engine.reencrypt(None).unwrap();
        assert!(stats.sstables_rewritten >= 1);
        // Marker is gone after migration to plaintext.
        assert!(!dir.join(crate::encryption::ENCRYPTION_MARKER_FILENAME).exists());
        assert_visible(&mut engine, &ids);
        engine.close().unwrap();

        // Plaintext reopen now works without any key.
        let mut engine = Engine::open(&dir).unwrap();
        assert_visible(&mut engine, &ids);
        engine.close().unwrap();

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reencrypt_same_state_is_idempotent_zero_stats() {
        let dir = temp_dir("reencrypt_idempotent");
        let mut engine = Engine::create_with_cipher(&dir, Some(cipher_a())).unwrap();
        seed_n_entities(&mut engine, 2);
        engine.flush().unwrap();

        let stats = engine.reencrypt(Some(&cipher_a())).unwrap();
        assert_eq!(stats, MigrationStats::default(), "no-op for matching cipher");
        // Transient marker NOT created on a no-op.
        assert!(!dir.join(crate::encryption::ENCRYPTION_MIGRATION_FILENAME).exists());
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reencrypt_plaintext_to_plaintext_is_idempotent() {
        let dir = temp_dir("reencrypt_plain_idempotent");
        let mut engine = Engine::create(&dir).unwrap();
        seed_n_entities(&mut engine, 2);
        let stats = engine.reencrypt(None).unwrap();
        assert_eq!(stats, MigrationStats::default());
        engine.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_refuses_when_encryption_next_marker_is_present() {
        let dir = temp_dir("reencrypt_partial_crash");
        let mut engine = Engine::create_with_cipher(&dir, Some(cipher_a())).unwrap();
        seed_n_entities(&mut engine, 1);
        engine.flush().unwrap();
        engine.close().unwrap();

        // Simulate a crashed reencrypt — manually drop the .encryption.next
        // marker on disk WITHOUT actually rewriting any file.
        std::fs::write(
            dir.join(crate::encryption::ENCRYPTION_MIGRATION_FILENAME),
            b"",
        )
        .unwrap();

        let err = Engine::open_with_cipher(&dir, Some(cipher_a())).unwrap_err();
        assert!(
            matches!(err, EngineError::EncryptionMigrationIncomplete { .. }),
            "expected EncryptionMigrationIncomplete, got {err:?}"
        );

        // Cleanup: remove the marker, open should succeed again.
        std::fs::remove_file(dir.join(crate::encryption::ENCRYPTION_MIGRATION_FILENAME)).unwrap();
        let engine = Engine::open_with_cipher(&dir, Some(cipher_a())).unwrap();
        engine.close().unwrap();

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reencrypt_refuses_when_prior_migration_marker_exists() {
        let dir = temp_dir("reencrypt_blocked");
        let mut engine = Engine::create_with_cipher(&dir, Some(cipher_a())).unwrap();
        seed_n_entities(&mut engine, 1);
        engine.flush().unwrap();

        // Plant a stale `.encryption.next` to simulate a prior crash.
        std::fs::write(
            dir.join(crate::encryption::ENCRYPTION_MIGRATION_FILENAME),
            b"",
        )
        .unwrap();

        let err = engine.reencrypt(Some(&cipher_b())).unwrap_err();
        assert!(matches!(err, EngineError::EncryptionMigrationIncomplete { .. }));
        engine.close().unwrap();

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reencrypt_round_trip_plain_to_enc_to_plain_preserves_records() {
        let dir = temp_dir("reencrypt_full_round_trip");
        let mut engine = Engine::create(&dir).unwrap();
        let ids = seed_n_entities(&mut engine, 7);
        engine.flush().unwrap();

        engine.reencrypt(Some(&cipher_a())).unwrap();
        assert_visible(&mut engine, &ids);
        engine.reencrypt(Some(&cipher_b())).unwrap();
        assert_visible(&mut engine, &ids);
        engine.reencrypt(None).unwrap();
        assert_visible(&mut engine, &ids);
        engine.close().unwrap();

        // Final state: plaintext. Re-open with no key works.
        let mut engine = Engine::open(&dir).unwrap();
        assert_visible(&mut engine, &ids);
        engine.close().unwrap();

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Regression: `snapshot_iter_streaming` MUST respect cross-kind
    /// tombstones. Before this fix, the merge grouped versions by full
    /// `(kind, primary)` SSTableKey — entity records and their
    /// tombstones lived under different kind bytes and never reached
    /// the per-key MVCC resolve together, so /iter kept emitting
    /// records that `snapshot_read` correctly reported as `Deleted`.
    /// This bit the bench-race aggregator (it filtered by entity
    /// `tx_id_supersede` but tombstone records leave that field
    /// untouched).
    #[test]
    fn snapshot_iter_streaming_respects_cross_kind_tombstones() {
        let dir = temp_dir("iter_tombstone");
        let mut engine = Engine::create(&dir).unwrap();
        // 3 entities, then delete the middle one.
        let eids: Vec<EntityId> = (0..3).map(|_| EntityId::now_v7()).collect();
        for &eid in &eids {
            let mut txn = engine.begin_write();
            txn.put_entity(make_entity(eid, "alive"));
            txn.commit().unwrap();
        }
        {
            let mut txn = engine.begin_write();
            txn.delete(eids[1].into_uuid());
            txn.commit().unwrap();
        }

        let snap = TxId::new(engine.manifest().last_tx_id);
        let visible: Vec<uuid::Uuid> = engine
            .snapshot_iter_streaming(snap)
            .filter_map(Result::ok)
            .filter_map(|r| match r {
                Record::Entity(e) => Some(e.entity_id.into_uuid()),
                _ => None,
            })
            .collect();
        // Tombstoned entity must NOT appear in the streaming iterator.
        assert!(visible.contains(&eids[0].into_uuid()));
        assert!(!visible.contains(&eids[1].into_uuid()), "tombstoned entity leaked through snapshot_iter_streaming");
        assert!(visible.contains(&eids[2].into_uuid()));

        // Same invariant after a flush — the tombstone lives in an
        // SSTable now, but the pre-scan still picks it up.
        engine.flush().unwrap();
        let snap = TxId::new(engine.manifest().last_tx_id);
        let visible: Vec<uuid::Uuid> = engine
            .snapshot_iter_streaming(snap)
            .filter_map(Result::ok)
            .filter_map(|r| match r {
                Record::Entity(e) => Some(e.entity_id.into_uuid()),
                _ => None,
            })
            .collect();
        assert!(!visible.contains(&eids[1].into_uuid()), "tombstoned entity leaked after flush");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
