//! Thread-safe wrapper around [`Engine`].
#![allow(clippy::doc_markdown)]
//!
//! v1.3 (and earlier) required callers to wrap `Engine` in
//! `Arc<Mutex<Engine>>` externally. v2.0 adds [`SharedEngine`] — same
//! semantics, but the mutex lives inside the wrapper. Server-side
//! code that wants to share an engine across worker threads now uses
//! `Arc<SharedEngine>` directly.
//!
//! Design choice (locked):
//!
//! - **Internal `Mutex<Engine>`, NOT `RwLock`.** Engine state mutates
//!   on commit + on snapshot reads (which touch a per-call cache).
//!   `RwLock` would only help if the read path were truly read-only,
//!   which it isn't today. v3 may refactor toward per-index locks for
//!   genuine read concurrency.
//! - **Closure-based write API.** `with_write_txn` takes a closure
//!   that receives a `WriteTxn<'_>` with a guard-tied lifetime. This
//!   avoids the self-referential-struct lifetime juggling of
//!   "return a handle that holds both the guard and the txn." Writers
//!   serialize automatically; concurrent attempts queue on the mutex.
//! - **Read methods mirror Engine.** Each `Engine` read method has a
//!   thin pass-through here that locks, calls, releases.
//!
//! Crash semantics, durability, and on-disk format are unchanged —
//! `SharedEngine` is purely a concurrency wrapper.

use std::path::Path;
use std::sync::Mutex;

use crate::engine::{
    CompactionStats, Engine, EngineError, IsolationLevel, RetentionPolicy, WriteTxn,
};
use crate::id::{EntityId, HyperedgeId, PropertyId, TxId, TypeId};
use crate::index::Distance;
use crate::mvcc::Resolved;
use crate::record::Record;
use crate::value::Value;

/// Multi-threaded wrapper around an `Engine`. Cheap to `Arc::clone`;
/// each call acquires the internal mutex briefly.
#[derive(Debug)]
pub struct SharedEngine {
    inner: Mutex<Engine>,
}

impl SharedEngine {
    /// Create a fresh database directory and wrap an Engine for it.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, EngineError> {
        Ok(Self {
            inner: Mutex::new(Engine::create(path)?),
        })
    }

    /// Open an existing database directory.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, EngineError> {
        Ok(Self {
            inner: Mutex::new(Engine::open(path)?),
        })
    }

    /// Wrap an existing `Engine`.
    #[must_use]
    pub fn from_engine(engine: Engine) -> Self {
        Self {
            inner: Mutex::new(engine),
        }
    }

    /// Unwrap back to the bare `Engine`. Panics if the mutex is poisoned.
    #[must_use]
    pub fn into_engine(self) -> Engine {
        self.inner.into_inner().expect("engine mutex poisoned")
    }

    /// Borrow the underlying mutex for tests / advanced patterns. Most
    /// callers should use the typed methods on `SharedEngine` instead.
    #[must_use]
    pub fn raw_mutex(&self) -> &Mutex<Engine> {
        &self.inner
    }

    // -----------------------------------------------------------------
    // Write surface (closure-based — writer holds the lock for the
    // duration of the closure body)
    // -----------------------------------------------------------------

    /// Execute a write transaction. The closure receives a `WriteTxn`
    /// bound to the lifetime of the internal lock; it must commit or
    /// rollback before returning. Other writers block until the closure
    /// returns.
    ///
    /// # Errors
    ///
    /// Propagates whatever the closure returns. The closure typically
    /// calls `txn.commit()` and surfaces the `EngineError` from that.
    pub fn with_write_txn<F, R>(&self, f: F) -> Result<R, EngineError>
    where
        F: FnOnce(WriteTxn<'_>) -> Result<R, EngineError>,
    {
        let mut engine = self.inner.lock().expect("engine mutex poisoned");
        let txn = engine.begin_write();
        f(txn)
    }

    /// Like `with_write_txn` but pre-sets the isolation level on the txn.
    pub fn with_write_txn_isolation<F, R>(
        &self,
        level: IsolationLevel,
        f: F,
    ) -> Result<R, EngineError>
    where
        F: FnOnce(WriteTxn<'_>) -> Result<R, EngineError>,
    {
        self.with_write_txn(|txn| f(txn.with_isolation(level)))
    }

    // -----------------------------------------------------------------
    // Read pass-throughs — each grabs the mutex, calls, releases
    // -----------------------------------------------------------------

    /// `Engine::snapshot_read` over the shared engine.
    pub fn snapshot_read(
        &self,
        uuid: &uuid::Uuid,
        snapshot: TxId,
    ) -> Result<Resolved<Record>, EngineError> {
        let mut e = self.inner.lock().expect("engine mutex poisoned");
        e.snapshot_read(uuid, snapshot)
    }

    /// `Engine::snapshot_iter` over the shared engine. Materialises into a `Vec`.
    pub fn snapshot_iter(&self, snapshot: TxId) -> Result<Vec<Record>, EngineError> {
        let mut e = self.inner.lock().expect("engine mutex poisoned");
        e.snapshot_iter(snapshot)
    }

    /// Manifest snapshot (cloned out of the lock so the caller is free
    /// of the mutex when it returns).
    #[must_use]
    pub fn manifest_snapshot(&self) -> crate::db::Manifest {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .manifest()
            .clone()
    }

    /// Number of open SSTables.
    #[must_use]
    pub fn sstable_count(&self) -> usize {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .sstable_count()
    }

    /// `Engine::lookup_by_external_key`.
    #[must_use]
    pub fn lookup_by_external_key(
        &self,
        property_id: PropertyId,
        value: &Value,
    ) -> Option<EntityId> {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .lookup_by_external_key(property_id, value)
    }

    /// `Engine::hyperedges_for_entity`.
    #[must_use]
    pub fn hyperedges_for_entity(&self, entity: EntityId) -> Vec<HyperedgeId> {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .hyperedges_for_entity(entity)
    }

    /// `Engine::hyperedges_by_type`.
    #[must_use]
    pub fn hyperedges_by_type(&self, type_id: TypeId) -> Vec<HyperedgeId> {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .hyperedges_by_type(type_id)
    }

    /// `Engine::property_lookup`.
    #[must_use]
    pub fn property_lookup(
        &self,
        type_id: TypeId,
        property_id: PropertyId,
        value: &Value,
    ) -> Vec<EntityId> {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .property_lookup(type_id, property_id, value)
    }

    /// `Engine::property_range`.
    #[must_use]
    pub fn property_range(
        &self,
        type_id: TypeId,
        property_id: PropertyId,
        low: Option<&Value>,
        high: Option<&Value>,
    ) -> Vec<EntityId> {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .property_range(type_id, property_id, low, high)
    }

    /// `Engine::vector_search`.
    #[must_use]
    pub fn vector_search(
        &self,
        property_id: PropertyId,
        query: &[f32],
        k: usize,
        metric: Distance,
    ) -> Vec<(EntityId, f32)> {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .vector_search(property_id, query, k, metric)
    }

    /// `Engine::tx_at_or_before`.
    #[must_use]
    pub fn tx_at_or_before(&self, timestamp_us: i64) -> Option<TxId> {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .tx_at_or_before(timestamp_us)
    }

    /// `Engine::commit_timestamp_us`.
    #[must_use]
    pub fn commit_timestamp_us(&self, tx_id: TxId) -> Option<i64> {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .commit_timestamp_us(tx_id)
    }

    /// `Engine::retention_policy`.
    #[must_use]
    pub fn retention_policy(&self, type_id: TypeId) -> RetentionPolicy {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .retention_policy(type_id)
    }

    // -----------------------------------------------------------------
    // Admin pass-throughs (acquire mutex, mutate, release)
    // -----------------------------------------------------------------

    /// `Engine::register_lookup_key`.
    pub fn register_lookup_key(&self, property_id: PropertyId) {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .register_lookup_key(property_id);
    }

    /// `Engine::register_vector_property`.
    pub fn register_vector_property(&self, property_id: PropertyId) {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .register_vector_property(property_id);
    }

    /// `Engine::register_property_btree`.
    pub fn register_property_btree(&self, type_id: TypeId, property_id: PropertyId) {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .register_property_btree(type_id, property_id);
    }

    /// `Engine::require_property`.
    pub fn require_property(&self, type_id: TypeId, property_id: PropertyId) {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .require_property(type_id, property_id);
    }

    /// `Engine::expect_value_tag`.
    pub fn expect_value_tag(&self, type_id: TypeId, property_id: PropertyId, tag: u8) {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .expect_value_tag(type_id, property_id, tag);
    }

    /// `Engine::set_retention_policy`.
    pub fn set_retention_policy(&self, type_id: TypeId, policy: RetentionPolicy) {
        self.inner
            .lock()
            .expect("engine mutex poisoned")
            .set_retention_policy(type_id, policy);
    }

    /// `Engine::flush`.
    pub fn flush(&self) -> Result<(), EngineError> {
        self.inner.lock().expect("engine mutex poisoned").flush()
    }

    /// `Engine::compact`.
    pub fn compact(&self) -> Result<CompactionStats, EngineError> {
        self.inner.lock().expect("engine mutex poisoned").compact()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::EntityRecord;
    use std::sync::Arc;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ndb-shared-{name}-{}", uuid::Uuid::now_v7()))
    }

    fn make_entity(name: &str) -> EntityRecord {
        EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(1), Value::String(name.into()))],
        }
    }

    #[test]
    fn single_threaded_write_then_read_round_trip() {
        let dir = temp_dir("single");
        let eng = SharedEngine::create(&dir).unwrap();
        let tx = eng
            .with_write_txn(|mut txn| {
                txn.put_entity(make_entity("alice"));
                txn.commit()
            })
            .unwrap();
        assert!(tx.get() > 0);
        let snap = TxId::new(eng.manifest_snapshot().last_tx_id);
        assert!(snap.get() >= tx.get());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_writers_all_succeed_with_unique_monotone_tx_ids() {
        // 4 threads × 25 commits each. All must complete, all tx_ids must
        // be distinct, the maximum must equal the count.
        let dir = temp_dir("concurrent");
        let eng = Arc::new(SharedEngine::create(&dir).unwrap());
        let n_threads = 4_usize;
        let per_thread = 25_usize;
        let mut handles = Vec::new();
        for t in 0..n_threads {
            let e = Arc::clone(&eng);
            handles.push(std::thread::spawn(move || {
                let mut my_txs = Vec::new();
                for i in 0..per_thread {
                    let tx = e
                        .with_write_txn(|mut txn| {
                            txn.put_entity(make_entity(&format!("t{t}-{i}")));
                            txn.commit()
                        })
                        .unwrap();
                    my_txs.push(tx.get());
                }
                my_txs
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        // Every commit got a tx_id.
        assert_eq!(all.len(), n_threads * per_thread);
        // Tx_ids are unique.
        let unique: std::collections::HashSet<_> = all.iter().copied().collect();
        assert_eq!(unique.len(), all.len(), "tx_ids must be unique");
        // Manifest's last_tx_id matches the highest seen.
        let max_seen = all.iter().copied().max().unwrap();
        let last = eng.manifest_snapshot().last_tx_id;
        assert_eq!(last, max_seen, "manifest tracks highest committed tx_id");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_readers_with_writer_dont_deadlock() {
        let dir = temp_dir("rw-mix");
        let eng = Arc::new(SharedEngine::create(&dir).unwrap());
        // Pre-populate.
        for _ in 0..5 {
            eng.with_write_txn(|mut txn| {
                txn.put_entity(make_entity("pre"));
                txn.commit()
            })
            .unwrap();
        }
        let writer = {
            let e = Arc::clone(&eng);
            std::thread::spawn(move || {
                for _ in 0..20 {
                    e.with_write_txn(|mut txn| {
                        txn.put_entity(make_entity("w"));
                        txn.commit()
                    })
                    .unwrap();
                }
            })
        };
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let e = Arc::clone(&eng);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let snap = TxId::new(e.manifest_snapshot().last_tx_id);
                        let _ = e.snapshot_iter(snap).unwrap();
                    }
                })
            })
            .collect();
        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
