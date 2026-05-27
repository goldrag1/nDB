//! Secondary indexes — the six mandatory v1 indexes (§14.2 / §17.1).
#![allow(clippy::doc_markdown)] // "SSTable", "HNSW", "ReBAC" used liberally.
//!
//! v1 index strategy, locked here:
//!
//! - **In-memory + rebuild on open.** Every index is maintained in process
//!   memory. On `Engine::open`, every index is rebuilt by replaying the
//!   primary store (SSTables in MANIFEST order, then memtable from WAL
//!   replay). Trade-off: O(N) startup time vs zero index-on-disk
//!   complexity. v2 will persist index sidecars; v1 keeps the
//!   write-path simple by not having to keep an extra file durable.
//! - **Updated on commit.** `WriteTxn::commit` calls `Engine::on_committed`
//!   which routes each record to every active index's `apply` method.
//!   This happens AFTER the WAL is durable but BEFORE the memtable
//!   insert returns — so a snapshot read in the same transaction sees
//!   the index update.
//! - **Tombstones tracked separately.** When a tombstone for `uuid` is
//!   committed, every index removes the entry pointing at that uuid.
//!   A re-assertion after a tombstone re-adds the entry.
//! - **MVCC visibility is the caller's job for v1.** Indexes return *all*
//!   live entries (across all tx_ids). Snapshot-aware indexing — where
//!   `lookup_by_external_key("ACME", snapshot=42)` honors the snapshot —
//!   is deferred to v2 when persisted indexes can carry per-entry tx
//!   metadata. v1 indexes are the "latest authoritative answer"
//!   semantics, which matches the dominant use case.
//!
//! The mandatory six (§17.1):
//!
//! 1. **Entity-by-ID** — implicit in `(kind=Entity, primary=uuid)` SSTable
//!    sort. Block index sidecar gives O(log N); deferred to a separate
//!    commit.
//! 2. **Hyperedge-by-ID** — same shape as #1.
//! 3. **Lookup-key reverse** — `(property_id, value bytes) → entity_id`.
//!    Implemented in [`lookup_key`].
//! 4. **Adjacency list** — `entity_id → [hyperedge ids referencing it]`.
//!    Implemented in [`adjacency`].
//! 5. **Hyperedge-type clustering** — `type_id → [hyperedge ids of that
//!    type]`. Implemented in [`type_cluster`].
//! 6. **Schema-driven property B-tree** — `(type_id, property_id, value)
//!    → entity_id`. Deferred to a later commit (depends on Value
//!    ordering semantics that aren't yet pinned).

use crate::id::TxId;
use crate::record::Record;

pub mod adjacency;
pub mod lookup_key;
pub mod property_btree;
pub mod type_cluster;
pub mod vector;

/// What an index does when a record is committed. Implementors mutate
/// their internal state to reflect the new record (or the deletion, for
/// tombstones).
pub trait Index {
    /// Apply a freshly-committed record to the index. Called once per
    /// record in `WriteTxn::commit`, after WAL durability.
    fn apply(&mut self, record: &Record, tx_id: TxId);

    /// Reset internal state to empty. Used by `Engine::open` before
    /// replaying the primary store + WAL.
    fn clear(&mut self);

    /// Short human-readable name, for diagnostics.
    fn name(&self) -> &'static str;
}

pub use adjacency::AdjacencyIndex;
pub use lookup_key::LookupKeyIndex;
pub use property_btree::PropertyBTreeIndex;
pub use type_cluster::HyperEdgeTypeIndex;
pub use vector::{Distance, VectorIndex};
