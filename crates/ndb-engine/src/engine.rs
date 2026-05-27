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
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::db::{Database, DatabaseError, Manifest, ManifestEntry};
use crate::error::EncodeError;
use crate::id::{EntityId, HyperedgeId, PropertyId, TX_ACTIVE, TxId, TypeId};
use crate::index::{AdjacencyIndex, HyperEdgeTypeIndex, Index, LookupKeyIndex};
use crate::memtable::Memtable;
use crate::mvcc::{Resolved, resolve_iter, visible_at};
use crate::record::{EntityRecord, HyperEdgeRecord, Record, TombstoneRecord};
use crate::sstable::{SSTableError, SSTableKey, SSTableReader, SSTableWriter};
use crate::value::Value;
use crate::wal::{WalReadError, WalReader, WriteAheadLog, truncate_to};

const WAL_FILENAME_SUFFIX: &str = ".ndblog";
const SSTABLE_FILENAME_SUFFIX: &str = ".ndb";

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
    /// Database directory handle (owns the LOCK + current MANIFEST).
    db: Database,
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
            db,
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
            db,
        };
        // Indexes are in-memory in v1 — rebuild them from the primary
        // store (SSTables in newest-first order) and the memtable
        // (already populated from WAL replay).
        engine.rebuild_indexes()?;
        Ok(engine)
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
        // SSTables (sstables[0] is newest layer; iterate in declared order).
        for sst in &mut self.sstables {
            for item in sst.iter() {
                let (rec, _) = item?;
                let tx = match &rec {
                    Record::Entity(e) => e.tx_id_assert,
                    Record::HyperEdge(h) => h.tx_id_assert,
                    Record::Tombstone(t) => t.tx_id_supersede,
                    _ => TxId::new(0),
                };
                self.lookup_key.apply(&rec, tx);
                self.adjacency.apply(&rec, tx);
                self.type_cluster.apply(&rec, tx);
            }
        }
        // Memtable.
        for (_k, rec) in self.memtable.iter() {
            let tx = match rec {
                Record::Entity(e) => e.tx_id_assert,
                Record::HyperEdge(h) => h.tx_id_assert,
                Record::Tombstone(t) => t.tx_id_supersede,
                _ => TxId::new(0),
            };
            self.lookup_key.apply(rec, tx);
            self.adjacency.apply(rec, tx);
            self.type_cluster.apply(rec, tx);
        }
        Ok(())
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
        let tx_id = TxId::new(self.db.allocate_tx_id());
        WriteTxn {
            engine: self,
            tx_id,
            pending: Vec::new(),
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

    /// Iterate every record visible at `snapshot`, in (kind, primary)
    /// order, deduplicating across memtable + SSTables. Useful for scans.
    /// O(N) — v1 has no block index.
    pub fn snapshot_iter(&mut self, snapshot: TxId) -> Result<Vec<Record>, EngineError> {
        // Collect all records across layers, group by SSTableKey, run
        // resolver per group.
        use std::collections::BTreeMap;
        let mut by_key: BTreeMap<SSTableKey, Vec<Record>> = BTreeMap::new();
        for (k, r) in self.memtable.iter() {
            by_key.entry(k.clone()).or_default().push(r.clone());
        }
        for sst in &mut self.sstables {
            for item in sst.iter() {
                let (rec, _) = item?;
                let k = SSTableKey::for_record(&rec);
                by_key.entry(k).or_default().push(rec);
            }
        }
        let mut out = Vec::new();
        for (_k, versions) in by_key {
            if let Some(r) = resolve_iter(versions.iter(), snapshot).into_live()
                && visible_at(r, snapshot)
            {
                out.push(r.clone());
            }
        }
        Ok(out)
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
}

impl WriteTxn<'_> {
    /// `TxId` allocated for this transaction.
    #[must_use]
    pub fn tx_id(&self) -> TxId {
        self.tx_id
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
    pub fn commit(self) -> Result<TxId, EngineError> {
        if self.pending.is_empty() {
            return Ok(self.tx_id);
        }
        let wal = self
            .engine
            .wal
            .as_mut()
            .expect("WAL active during commit (engine open invariant)");
        let records: Vec<Record> = self.pending;
        wal.append_batch(&records)?;
        wal.sync()?;
        // Memtable insert + index update happen AFTER WAL durability so a
        // crash before this point cleanly rolls back the transaction; a
        // crash AFTER WAL durability means the records are durable in the
        // log and will be replayed on the next open (which will repopulate
        // the in-memory state).
        for r in records {
            self.engine.lookup_key.apply(&r, self.tx_id);
            self.engine.adjacency.apply(&r, self.tx_id);
            self.engine.type_cluster.apply(&r, self.tx_id);
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
            Record::TypeName(_) | Record::RoleName(_) | Record::PropertyKey(_) => 0,
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
