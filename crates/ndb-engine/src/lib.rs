//! nDB engine — append-only LSM storage core for the nDB n-dimensional
//! hypergraph database.
//!
//! See `docs/superpowers/specs/2026-05-27-nDB-hypergraph-design.md` for the
//! authoritative architectural design. This crate implements §11 (Primary
//! Storage Format): byte-level record layouts, the self-describing tagged
//! `Value` union, identifier newtypes, and CRC-checked envelope handling.
//!
//! What lives here today:
//!
//! - `record` — 6 record kinds (entity, hyperedge, tombstone, three dictionary
//!   kinds) with encode + decode + envelope checks
//! - `value` — tagged-union property values, all 11 tags
//! - `id` — `EntityId`, `HyperedgeId`, `TypeId`, `RoleId`, `PropertyId`,
//!   `TxId`, plus the `TX_ACTIVE` and `TYPE_UNTYPED` sentinels
//! - `codec` — low-level little-endian read/write helpers
//! - `error` — `EncodeError` / `DecodeError` types
//!
//! What will live here next (not yet implemented):
//!
//! - Append-only log writer + reader (§9.1, `.ndblog`)
//! - `SSTable` structure + `MANIFEST` / `CURRENT` / `LOCK` (§11.5)
//! - Memtable + flush
//! - MVCC visibility checks (§10)
//! - Single-writer transaction commit (§14.3)
//! - The six mandatory v1 indexes (§14.2)

#![warn(missing_docs)]

pub mod block_index;
pub mod codec;
pub mod db;
pub mod encryption;
pub mod engine;
pub mod error;
pub mod id;
pub mod index;
pub mod memtable;
pub mod mvcc;
pub mod query;
pub mod record;
pub mod shared;
pub mod sstable;
pub mod validation;
pub mod value;
pub mod wal;
pub mod wire;
pub mod wire_query;

pub use encryption::{
    Cipher, DEFAULT_CHUNK_SIZE, ENCRYPTED_FILE_FORMAT_VERSION, ENCRYPTED_FILE_MAGIC,
    ENCRYPTION_ALGO_AES_GCM_256, ENCRYPTION_MARKER_FILENAME, ENCRYPTION_MARKER_FORMAT_VERSION,
    ENCRYPTION_MARKER_MAGIC, EncryptedFile, EncryptionError, EncryptionMarker,
    FINGERPRINT_LEN, KEY_LEN, NONCE_LEN, TAG_LEN,
};
pub use db::{
    CURRENT_FILE, Database, DatabaseError, LOCK_FILE, MANIFEST_FORMAT_VERSION,
    MANIFEST_FORMAT_VERSION_MAX_SUPPORTED, MANIFEST_MAGIC, MANIFEST_PREFIX, MAX_LSM_LEVEL,
    Manifest, ManifestEntry, manifest_filename, parse_manifest_filename,
};
pub use engine::{
    CompactionStats, Engine, EngineError, IsolationLevel, RetentionPolicy, WriteTxn,
};
pub use shared::SharedEngine;
pub use error::{DecodeError, EncodeError};
pub use id::{EntityId, HyperedgeId, PropertyId, RoleId, TX_ACTIVE, TYPE_UNTYPED, TxId, TypeId};
pub use index::{
    AdjacencyIndex, Distance, HyperEdgeTypeIndex, Index, LookupKeyIndex, PropertyBTreeIndex,
    VectorIndex,
};
pub use memtable::Memtable;
pub use mvcc::{Resolved, effective_tx, resolve, resolve_owned, visible_at};
pub use query::{Bindings, QueryError, execute as execute_query};
pub use record::{
    ENVELOPE_OVERHEAD, EntityRecord, FORMAT_VERSION, FORMAT_VERSION_MAX_SUPPORTED, HyperEdgeRecord,
    PropertyKeyRecord, Record, RecordKind, RoleNameRecord, TombstoneRecord, TypeNameRecord,
    peek_record_kind, peek_record_size,
};
pub use block_index::{
    BLOCK_INDEX_EXTENSION, BLOCK_INDEX_FORMAT_VERSION, BLOCK_INDEX_FORMAT_VERSION_MAX_SUPPORTED,
    BLOCK_INDEX_MAGIC, BlockIndex, BlockIndexEntry, BlockIndexError, BlockIndexWriter,
    DEFAULT_BLOCK_SIZE, load_sidecar, sidecar_path_for,
};
pub use sstable::{
    SSTABLE_EXTENSION, SSTABLE_FOOTER_SIZE, SSTABLE_FORMAT_VERSION,
    SSTABLE_FORMAT_VERSION_MAX_SUPPORTED, SSTABLE_MAGIC, SSTableError, SSTableFooter, SSTableIter,
    SSTableKey, SSTableReader, SSTableWriter, read_footer,
};
pub use validation::{
    CONSTRAINT_KIND_REQUIRED, CONSTRAINT_KIND_VALUE_TAG, PROP_CONSTRAINT_KIND, PROP_EXPECTED_TAG,
    PROP_TARGET_PROPERTY, PROP_TARGET_TYPE, TYPE_VALIDATION_CONSTRAINT, ValidationEngine,
    ValidationError,
};
pub use value::Value;
pub use wal::{WAL_EXTENSION, WalReadError, WalReader, WalRecovery, WriteAheadLog};
pub use wire::{
    CommitRequest, CommitResponse, ErrorResponse, JsonProperty, JsonRecord, JsonRole, JsonValue,
    LookupRequest, LookupResponse, PropertyLookupRequest, PropertyLookupResponse,
    PropertyRangeRequest, PropertyRangeResponse, ReadResponse, SubscribeRequest, TraverseHop,
    TraverseRequest, TraverseResponse, TxIdOrActive, VectorHit, VectorMetric, VectorSearchRequest,
    VectorSearchResponse, WireError,
};
pub use wire_query::{
    AsOf, CmpOp, DEFAULT_MAX_RECURSION_DEPTH, Expr, Pattern, PropertyFilter, QueryRequest,
    QueryResponse, Recursion, RoleBinding, Term,
};
