//! In-memory write buffer. Sits between transaction commit and the
//! SSTable flush. Holds the latest writes sorted by [`SSTableKey`] with
//! multiple versions per key (so MVCC reads can find the right version).
#![allow(clippy::doc_markdown)] // "SSTable" / "BTreeMap" used liberally as domain terms.
//!
//! v1 design decisions baked in here:
//!
//! - **`BTreeMap<SSTableKey, Vec<Record>>`.** Sorted-map for O(log n)
//!   point lookup and ordered iteration on flush. Vec for the multi-version
//!   chain. Inserts append, so iteration order within a key tracks
//!   `tx_id_assert` (monotonic per writer).
//! - **No locking inside `Memtable`.** Single-writer model (§14.3). The
//!   higher-level engine wraps a `Memtable` in whatever synchronisation it
//!   prefers; this struct itself is `&mut self` for writes.
//! - **Size estimate is conservative.** `size_bytes` counts the encoded
//!   length of every record's payload + the `BTreeMap` overhead per key.
//!   This is the trigger for size-based flush; precise accounting is not
//!   worth the complexity.
//! - **Flush consumes the memtable.** Calling [`Memtable::flush_into`]
//!   writes every record (in sorted order, multi-versions consecutive) into
//!   the provided [`SSTableWriter`] and drains the in-memory state. The
//!   caller is responsible for crash-safe MANIFEST updates after the
//!   writer's `finish()` returns.

use std::collections::BTreeMap;

use crate::error::EncodeError;
use crate::mvcc::{Resolved, resolve, resolve_iter};
use crate::record::{Record, RecordKind};
use crate::sstable::{SSTableError, SSTableKey, SSTableWriter};

/// Minimum size_bytes overhead attributed to each key (BTree node + Vec
/// header). Approximate; tuned to give realistic flush triggers on small
/// records.
const KEY_OVERHEAD_BYTES: u64 = 64;

/// In-memory sorted multi-version store.
#[derive(Debug, Default)]
pub struct Memtable {
    entries: BTreeMap<SSTableKey, Vec<Record>>,
    size_bytes: u64,
    record_count: u64,
}

impl Memtable {
    /// Construct an empty memtable.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a record. Multiple records for the same key are kept in
    /// insertion order. Returns `EncodeError` if the record can't be
    /// size-estimated (i.e. would also fail to flush) — caller should treat
    /// this as fatal and abort the transaction.
    pub fn insert(&mut self, record: Record) -> Result<(), EncodeError> {
        // Size estimate via a discarded encode buffer. Overhead is unavoidable
        // in v1; future micro-optimisation could implement a fast-path size()
        // method per record kind that doesn't allocate.
        let mut buf = Vec::new();
        record.encode(&mut buf)?;
        let key = SSTableKey::for_record(&record);
        let entry = self.entries.entry(key).or_insert_with(|| {
            self.size_bytes += KEY_OVERHEAD_BYTES;
            Vec::new()
        });
        entry.push(record);
        self.size_bytes += buf.len() as u64;
        self.record_count += 1;
        Ok(())
    }

    /// Number of records currently held (across all keys and versions).
    #[must_use]
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Number of distinct keys.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.entries.len()
    }

    /// Estimated in-memory footprint, in bytes. Used by the higher-level
    /// engine to trigger flush.
    #[must_use]
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// True when no records have been inserted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.record_count == 0
    }

    /// Borrow every version stored for `key`, in insertion order.
    #[must_use]
    pub fn versions(&self, key: &SSTableKey) -> Option<&[Record]> {
        self.entries.get(key).map(Vec::as_slice)
    }

    /// Resolve a snapshot read for `key`. Returns the MVCC winner among the
    /// in-memory versions only; SSTable lookup happens at a higher layer.
    #[must_use]
    pub fn lookup(&self, key: &SSTableKey, snapshot: crate::id::TxId) -> Resolved<&Record> {
        match self.entries.get(key) {
            Some(versions) => resolve(versions, snapshot),
            None => Resolved::Missing,
        }
    }

    /// Resolve a snapshot read for a UUID across all three UUID-bearing
    /// record kinds (`Entity`, `HyperEdge`, `Tombstone`).
    ///
    /// The primary store sorts records by `(record_kind, primary_id)`, so an
    /// entity and a tombstone for the same UUID land at different
    /// [`SSTableKey`]s. MVCC resolution must consider both: a tombstone for
    /// the same UUID supersedes a live entity. This method aggregates
    /// versions across the three relevant kinds and runs the visibility
    /// resolver on the union.
    #[must_use]
    pub fn lookup_by_uuid(
        &self,
        uuid: &uuid::Uuid,
        snapshot: crate::id::TxId,
    ) -> Resolved<&Record> {
        let primary = uuid.as_bytes().to_vec();
        let mut buckets: [Option<&Vec<Record>>; 3] = [None, None, None];
        for (slot, kind) in [
            RecordKind::Entity,
            RecordKind::HyperEdge,
            RecordKind::Tombstone,
        ]
        .into_iter()
        .enumerate()
        {
            let key = SSTableKey {
                kind: kind.as_byte(),
                primary: primary.clone(),
            };
            buckets[slot] = self.entries.get(&key);
        }
        let it = buckets.into_iter().flatten().flat_map(Vec::as_slice);
        resolve_iter(it, snapshot)
    }

    /// Iterate every (key, record) pair in SSTable sort order — i.e. by
    /// `SSTableKey`, then by insertion order within a key (which matches
    /// `tx_id_assert` order under the single-writer assumption).
    pub fn iter(&self) -> impl Iterator<Item = (&SSTableKey, &Record)> {
        self.entries
            .iter()
            .flat_map(|(k, versions)| versions.iter().map(move |r| (k, r)))
    }

    /// Drain the memtable into `writer`, writing every record in sort order.
    /// The caller still owns the writer and must call [`SSTableWriter::finish`]
    /// to atomically publish the file.
    pub fn flush_into(&mut self, writer: &mut SSTableWriter) -> Result<u64, SSTableError> {
        let mut written = 0;
        // Drain in sorted order. `into_iter()` on BTreeMap is sorted; we
        // also exhaust each Vec so the memtable is empty afterwards.
        for (_key, versions) in std::mem::take(&mut self.entries) {
            for record in versions {
                writer.append(&record)?;
                written += 1;
            }
        }
        self.size_bytes = 0;
        self.record_count = 0;
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId};
    use crate::record::{
        EntityRecord, HyperEdgeRecord, PropertyKeyRecord, RoleNameRecord, TombstoneRecord,
        TypeNameRecord,
    };
    use crate::sstable::SSTableReader;
    use crate::value::Value;

    fn entity(eid: EntityId, type_id: u32, tx: u64) -> Record {
        Record::Entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(type_id),
            tx_id_assert: TxId::new(tx),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(1), Value::String(format!("v{tx}")))],
        })
    }
    fn tombstone(uuid: uuid::Uuid, tx: u64) -> Record {
        Record::Tombstone(TombstoneRecord {
            target_id: uuid,
            tx_id_supersede: TxId::new(tx),
        })
    }
    fn dict(id: u32, name: &str) -> Record {
        Record::TypeName(TypeNameRecord {
            id: TypeId::new(id),
            name: name.into(),
        })
    }

    #[test]
    fn insert_and_lookup_latest() {
        let mut m = Memtable::new();
        let eid = EntityId::now_v7();
        m.insert(entity(eid, 1, 10)).unwrap();
        m.insert(entity(eid, 1, 20)).unwrap();
        let key = SSTableKey::for_record(&entity(eid, 1, 10));
        match m.lookup(&key, TxId::new(15)) {
            Resolved::Live(r) => {
                let Record::Entity(e) = r else { unreachable!() };
                assert_eq!(e.tx_id_assert.get(), 10);
            }
            other => panic!("expected Live, got {other:?}"),
        }
        match m.lookup(&key, TxId::new(20)) {
            Resolved::Live(r) => {
                let Record::Entity(e) = r else { unreachable!() };
                assert_eq!(e.tx_id_assert.get(), 20);
            }
            other => panic!("expected Live(tx=20), got {other:?}"),
        }
    }

    #[test]
    fn tombstone_makes_entity_deleted_via_lookup_by_uuid() {
        // lookup_by_uuid aggregates Entity + Tombstone records sharing the
        // same UUID across their separate (kind, primary) buckets.
        let mut m = Memtable::new();
        let eid = EntityId::now_v7();
        m.insert(entity(eid, 1, 5)).unwrap();
        m.insert(tombstone(eid.into_uuid(), 10)).unwrap();
        let uuid = eid.into_uuid();
        assert!(matches!(
            m.lookup_by_uuid(&uuid, TxId::new(10)),
            Resolved::Deleted { .. }
        ));
        assert!(matches!(
            m.lookup_by_uuid(&uuid, TxId::new(5)),
            Resolved::Live(_)
        ));
    }

    #[test]
    fn lookup_by_uuid_missing_for_unknown_uuid() {
        let m = Memtable::new();
        assert!(matches!(
            m.lookup_by_uuid(&uuid::Uuid::now_v7(), TxId::new(100)),
            Resolved::Missing
        ));
    }

    #[test]
    fn lookup_by_uuid_finds_hyperedge() {
        let mut m = Memtable::new();
        let hid = HyperedgeId::now_v7();
        m.insert(Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: hid,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(20),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(1), EntityId::now_v7())],
            properties: vec![],
        }))
        .unwrap();
        match m.lookup_by_uuid(&hid.into_uuid(), TxId::new(20)) {
            Resolved::Live(Record::HyperEdge(h)) => assert_eq!(h.hyperedge_id, hid),
            other => panic!("expected Live(HyperEdge), got {other:?}"),
        }
    }

    #[test]
    fn iter_yields_keys_in_sort_order() {
        let mut m = Memtable::new();
        // Insert a mix of kinds; iter() must yield by (kind, primary).
        m.insert(dict(2, "B")).unwrap();
        m.insert(dict(1, "A")).unwrap();
        m.insert(Record::PropertyKey(PropertyKeyRecord {
            id: PropertyId::new(5),
            name: "x".into(),
        }))
        .unwrap();
        m.insert(Record::RoleName(RoleNameRecord {
            id: RoleId::new(3),
            name: "approver".into(),
        }))
        .unwrap();

        let keys: Vec<_> = m.iter().map(|(k, _)| k.clone()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "iter must yield in SSTableKey order");
    }

    #[test]
    fn flush_to_sstable_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "ndb-memtable-{}-{}",
            "flush",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("000001.ndb");

        let mut m = Memtable::new();
        // Mix entity + hyperedge + dictionary + tombstone, all sortable.
        let eid = EntityId::now_v7();
        let hid = HyperedgeId::now_v7();
        m.insert(dict(1, "Customer")).unwrap();
        m.insert(dict(2, "Supplier")).unwrap();
        m.insert(entity(eid, 1, 10)).unwrap();
        m.insert(entity(eid, 1, 20)).unwrap(); // second version of same key
        m.insert(Record::HyperEdge(HyperEdgeRecord {
            hyperedge_id: hid,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(15),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(1), EntityId::now_v7())],
            properties: vec![],
        }))
        .unwrap();
        let total_records = m.record_count();
        assert!(m.size_bytes() > 0);

        let mut w = SSTableWriter::create(&path).unwrap();
        let written = m.flush_into(&mut w).unwrap();
        let footer = w.finish().unwrap();
        assert_eq!(written, total_records);
        assert_eq!(footer.record_count, total_records);

        // After flush, memtable is empty.
        assert!(m.is_empty());
        assert_eq!(m.size_bytes(), 0);

        // SSTable round-trip — every record is readable.
        let r = SSTableReader::open(&path).unwrap();
        let read_back: Result<Vec<_>, _> = r.iter().map(|res| res.map(|(rec, _)| rec)).collect();
        let read_back = read_back.unwrap();
        assert_eq!(read_back.len() as u64, total_records);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn size_bytes_tracks_insertions() {
        let mut m = Memtable::new();
        assert_eq!(m.size_bytes(), 0);
        m.insert(dict(1, "A")).unwrap();
        let s1 = m.size_bytes();
        assert!(s1 > 0);
        m.insert(dict(2, "B")).unwrap();
        assert!(m.size_bytes() > s1);
    }

    #[test]
    fn versions_in_insertion_order() {
        let mut m = Memtable::new();
        let eid = EntityId::now_v7();
        for tx in [10u64, 20, 30] {
            m.insert(entity(eid, 1, tx)).unwrap();
        }
        let key = SSTableKey::for_record(&entity(eid, 1, 10));
        let vs = m.versions(&key).unwrap();
        let txs: Vec<u64> = vs
            .iter()
            .filter_map(|r| {
                if let Record::Entity(e) = r {
                    Some(e.tx_id_assert.get())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(txs, vec![10, 20, 30]);
    }
}
