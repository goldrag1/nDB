//! Thread-safe wrapper around [`Engine`].
#![allow(clippy::doc_markdown)]
//!
//! v1.3 (and earlier) required callers to wrap `Engine` in
//! `Arc<Mutex<Engine>>` externally. v2.0 adds [`SharedEngine`] — same
//! semantics, but the lock lives inside the wrapper. Server-side
//! code that wants to share an engine across worker threads now uses
//! `Arc<SharedEngine>` directly.
//!
//! Design choice (locked):
//!
//! - **Internal `RwLock<Engine>`.** As of v3-final the engine's read
//!   methods (`snapshot_read`, `snapshot_iter`, every index lookup,
//!   `vector_search`) all take `&self` — the SSTable readers are
//!   mmap-backed and the in-memory indexes only mutate on commit. Read
//!   methods acquire `.read()` (parallel) and writes acquire `.write()`
//!   (serialised). This unlocks genuine read concurrency for the
//!   bench's stress race + server's read-heavy workloads, which were
//!   queueing on the previous `Mutex<Engine>`.
//! - **Closure-based write API.** `with_write_txn` takes a closure
//!   that receives a `WriteTxn<'_>` with a guard-tied lifetime. This
//!   avoids the self-referential-struct lifetime juggling of
//!   "return a handle that holds both the guard and the txn." Writers
//!   serialise automatically; concurrent attempts queue on the
//!   `RwLock`'s writer slot.
//! - **Read methods mirror Engine.** Each `Engine` read method has a
//!   thin pass-through here that locks, calls, releases.
//!
//! Crash semantics, durability, and on-disk format are unchanged —
//! `SharedEngine` is purely a concurrency wrapper.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::engine::{
    CompactionPlan, CompactionStats, Engine, EngineError, IsolationLevel, RetentionPolicy, WriteTxn,
    merge_planned,
};
use crate::id::{EntityId, HyperedgeId, PropertyId, TxId, TypeId};
use crate::index::Distance;
use crate::mvcc::Resolved;
use crate::record::Record;
use crate::value::Value;

/// Multi-threaded wrapper around an `Engine`. Cheap to `Arc::clone`;
/// each call acquires the internal lock briefly.
#[derive(Debug)]
pub struct SharedEngine {
    inner: RwLock<Engine>,
    /// Active read snapshots: `tx_id → refcount`. A reader that pins
    /// snapshot T while iterating registers via `register_snapshot(T)`
    /// and releases via `release_snapshot(T)`. The compactor uses
    /// `oldest_active_snapshot()` as a floor — versions superseded
    /// before that tx are safe to drop.
    snapshots: Mutex<BTreeMap<TxId, usize>>,
    /// Serialises compactions. The off-lock path runs its merge without the
    /// engine write lock, so two concurrent compactions could otherwise pick
    /// overlapping input sets; this mutex makes at most one run at a time
    /// without blocking ordinary readers/writers.
    compaction_lock: Mutex<()>,
}

impl SharedEngine {
    /// Create a fresh database directory and wrap an Engine for it.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, EngineError> {
        Ok(Self {
            inner: RwLock::new(Engine::create(path)?),
            snapshots: Mutex::new(BTreeMap::new()),
            compaction_lock: Mutex::new(()),
        })
    }

    /// Open an existing database directory.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, EngineError> {
        Ok(Self {
            inner: RwLock::new(Engine::open(path)?),
            snapshots: Mutex::new(BTreeMap::new()),
            compaction_lock: Mutex::new(()),
        })
    }

    /// Wrap an existing `Engine`.
    #[must_use]
    pub fn from_engine(engine: Engine) -> Self {
        Self {
            inner: RwLock::new(engine),
            snapshots: Mutex::new(BTreeMap::new()),
            compaction_lock: Mutex::new(()),
        }
    }

    /// Unwrap back to the bare `Engine`. Panics if the lock is poisoned.
    #[must_use]
    pub fn into_engine(self) -> Engine {
        self.inner.into_inner().expect("engine lock poisoned")
    }

    /// Borrow the underlying lock for tests / advanced patterns. Most
    /// callers should use the typed methods on `SharedEngine` instead.
    #[must_use]
    pub fn raw_lock(&self) -> &RwLock<Engine> {
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
        let mut engine = self.inner.write().expect("engine lock poisoned");
        // Backpressure: reject before doing any work if flushes are outpacing
        // compaction (no-op unless `l0_stall_threshold` is configured).
        engine.check_write_admission()?;
        let txn = engine.begin_write();
        let result = f(txn)?;
        // Bound resident write memory: flush the memtable if it has grown past
        // `memtable_flush_threshold_bytes` (no-op when disabled). Composes with
        // off-lock compaction — the flushed table is preserved by its
        // set-based install.
        engine.auto_flush_if_full()?;
        Ok(result)
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
        let e = self.inner.read().expect("engine lock poisoned");
        e.snapshot_read(uuid, snapshot)
    }

    /// `Engine::snapshot_iter` over the shared engine. Materialises into a `Vec`.
    pub fn snapshot_iter(&self, snapshot: TxId) -> Result<Vec<Record>, EngineError> {
        let e = self.inner.read().expect("engine lock poisoned");
        e.snapshot_iter(snapshot)
    }

    /// `Engine::versions_of` over the shared engine: the full version chain of
    /// one key, oldest first, each paired with its effective tx.
    ///
    /// # Errors
    /// Propagates SSTable read errors.
    pub fn versions_of(&self, uuid: &uuid::Uuid) -> Result<Vec<(TxId, Record)>, EngineError> {
        let e = self.inner.read().expect("engine lock poisoned");
        e.versions_of(uuid)
    }

    /// Manifest snapshot (cloned out of the lock so the caller is free
    /// of the lock when it returns).
    #[must_use]
    pub fn manifest_snapshot(&self) -> crate::db::Manifest {
        self.inner
            .read()
            .expect("engine lock poisoned")
            .manifest()
            .clone()
    }

    /// Number of open SSTables.
    #[must_use]
    pub fn sstable_count(&self) -> usize {
        self.inner
            .read()
            .expect("engine lock poisoned")
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
            .read()
            .expect("engine lock poisoned")
            .lookup_by_external_key(property_id, value)
    }

    /// `Engine::hyperedges_for_entity`.
    #[must_use]
    pub fn hyperedges_for_entity(&self, entity: EntityId) -> Vec<HyperedgeId> {
        self.inner
            .read()
            .expect("engine lock poisoned")
            .hyperedges_for_entity(entity)
    }

    /// `Engine::hyperedges_by_type`.
    #[must_use]
    pub fn hyperedges_by_type(&self, type_id: TypeId) -> Vec<HyperedgeId> {
        self.inner
            .read()
            .expect("engine lock poisoned")
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
            .read()
            .expect("engine lock poisoned")
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
            .read()
            .expect("engine lock poisoned")
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
            .read()
            .expect("engine lock poisoned")
            .vector_search(property_id, query, k, metric)
    }

    /// `Engine::tx_at_or_before`.
    #[must_use]
    pub fn tx_at_or_before(&self, timestamp_us: i64) -> Option<TxId> {
        self.inner
            .read()
            .expect("engine lock poisoned")
            .tx_at_or_before(timestamp_us)
    }

    /// `Engine::commit_timestamp_us`.
    #[must_use]
    pub fn commit_timestamp_us(&self, tx_id: TxId) -> Option<i64> {
        self.inner
            .read()
            .expect("engine lock poisoned")
            .commit_timestamp_us(tx_id)
    }

    /// `Engine::retention_policy`.
    #[must_use]
    pub fn retention_policy(&self, type_id: TypeId) -> RetentionPolicy {
        self.inner
            .read()
            .expect("engine lock poisoned")
            .retention_policy(type_id)
    }

    // -----------------------------------------------------------------
    // Admin pass-throughs (acquire mutex, mutate, release)
    // -----------------------------------------------------------------

    /// `Engine::register_lookup_key`.
    pub fn register_lookup_key(&self, property_id: PropertyId) {
        self.inner
            .write()
            .expect("engine lock poisoned")
            .register_lookup_key(property_id);
    }

    /// `Engine::register_vector_property`.
    pub fn register_vector_property(&self, property_id: PropertyId) {
        self.inner
            .write()
            .expect("engine lock poisoned")
            .register_vector_property(property_id);
    }

    /// `Engine::register_property_btree`.
    pub fn register_property_btree(&self, type_id: TypeId, property_id: PropertyId) {
        self.inner
            .write()
            .expect("engine lock poisoned")
            .register_property_btree(type_id, property_id);
    }

    /// `Engine::require_property`.
    pub fn require_property(&self, type_id: TypeId, property_id: PropertyId) {
        self.inner
            .write()
            .expect("engine lock poisoned")
            .require_property(type_id, property_id);
    }

    /// `Engine::expect_value_tag`.
    pub fn expect_value_tag(&self, type_id: TypeId, property_id: PropertyId, tag: u8) {
        self.inner
            .write()
            .expect("engine lock poisoned")
            .expect_value_tag(type_id, property_id, tag);
    }

    /// `Engine::set_retention_policy`.
    pub fn set_retention_policy(&self, type_id: TypeId, policy: RetentionPolicy) {
        self.inner
            .write()
            .expect("engine lock poisoned")
            .set_retention_policy(type_id, policy);
    }

    /// `Engine::flush`.
    pub fn flush(&self) -> Result<(), EngineError> {
        self.inner.write().expect("engine lock poisoned").flush()
    }

    /// `Engine::compact`. Snapshot-aware: uses the oldest active
    /// snapshot (registered via `register_snapshot`) as the floor so
    /// in-flight readers don't lose versions out from under them. With
    /// no registered snapshots, behaves identically to v1.3 — drops
    /// everything superseded.
    pub fn compact(&self) -> Result<CompactionStats, EngineError> {
        self.compact_offlock()
    }

    /// Off-lock compaction: the read+merge+write phase runs **without** the
    /// engine write lock, so writers and flushes proceed concurrently; only
    /// the brief plan (phase 1) and install (phase 3) take the lock.
    ///
    /// Snapshot-aware like the locking compaction: it merges against the
    /// oldest active snapshot floor and **re-checks it before installing**,
    /// aborting (and discarding the output) if a reader registered an older
    /// snapshot while the merge ran — so an off-lock compaction can never
    /// drop a version a registered reader still needs. Serialised against
    /// other compactions by an internal mutex; concurrent calls run one at a
    /// time without blocking ordinary reads/writes.
    ///
    /// Returns no-op [`CompactionStats`] when there's nothing to compact
    /// (<2 SSTables) or when the run is safely aborted (floor regressed, or
    /// the input set changed underneath it).
    pub fn compact_offlock(&self) -> Result<CompactionStats, EngineError> {
        let floor = self.oldest_active_snapshot();
        run_offlock_compaction(&self.inner, &self.compaction_lock, floor, || {
            self.oldest_active_snapshot()
        })
    }

    /// Run a compaction **only if** the [`CompactionPolicy`] trigger is met
    /// (the live SSTable count has reached `l0_trigger`). Returns
    /// `Ok(Some(stats))` when a compaction ran, `Ok(None)` when the trigger
    /// wasn't met. Embedders that drive their own maintenance loop call this
    /// directly; the batteries-included alternative is
    /// [`spawn_auto_compactor`](Self::spawn_auto_compactor).
    pub fn maybe_compact(
        &self,
        policy: &CompactionPolicy,
    ) -> Result<Option<CompactionStats>, EngineError> {
        if self.sstable_count() >= policy.l0_trigger {
            self.compact().map(Some)
        } else {
            Ok(None)
        }
    }

    /// Spawn a background thread that automatically compacts when the
    /// [`CompactionPolicy`] trigger fires — closing the production gap where
    /// compaction otherwise only happens when an operator calls it by hand.
    ///
    /// The thread polls every `policy.check_interval` (in small sub-steps so
    /// it stops promptly), calling [`maybe_compact`](Self::maybe_compact). A
    /// compaction error is logged and the loop continues — a transient
    /// failure must not silently kill the compactor. The returned
    /// [`CompactorHandle`] stops the thread on `stop()` or on drop.
    ///
    /// Concurrency note: this **schedules** compaction; it does not yet make
    /// it contention-free. The compaction still takes the engine write lock
    /// for its duration (same as a manual `compact()`), so it serialises
    /// with writers. Moving the merge phase off the write lock — reading the
    /// immutable input SSTables by path and taking the lock only for the
    /// manifest swap — is the documented follow-on (see
    /// `docs/architecture/production-readiness.md`).
    #[must_use]
    pub fn spawn_auto_compactor(
        engine: &Arc<SharedEngine>,
        policy: CompactionPolicy,
    ) -> CompactorHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let eng = Arc::clone(engine);
        // Poll in <=100ms slices so stop() is responsive even with a long
        // check_interval.
        let slice = policy.check_interval.min(Duration::from_millis(100));
        let join = std::thread::Builder::new()
            .name("ndb-auto-compactor".into())
            .spawn(move || {
                let mut waited = Duration::ZERO;
                while !thread_stop.load(Ordering::Relaxed) {
                    if waited >= policy.check_interval {
                        waited = Duration::ZERO;
                        if let Err(e) = eng.maybe_compact(&policy) {
                            eprintln!("ndb-engine: auto-compactor error: {e}");
                        }
                    }
                    std::thread::sleep(slice);
                    waited += slice;
                }
            })
            .expect("spawn auto-compactor thread");
        CompactorHandle {
            stop,
            join: Some(join),
        }
    }

    // -----------------------------------------------------------------
    // Active-snapshot registry (snapshot-aware compaction support)
    // -----------------------------------------------------------------

    /// Register an active read snapshot. Subsequent compactions will
    /// treat this tx_id as a floor and refuse to drop versions newer
    /// than it. Pair with [`release_snapshot`] when done.
    ///
    /// Re-registering the same tx_id increments a refcount; release
    /// must be called the same number of times for the snapshot to
    /// drop out of the floor calculation.
    pub fn register_snapshot(&self, tx_id: TxId) {
        let mut s = self.snapshots.lock().expect("snapshot map poisoned");
        *s.entry(tx_id).or_insert(0) += 1;
    }

    /// Release a previously-registered snapshot. Decrements refcount;
    /// removes the entry when refcount reaches zero.
    pub fn release_snapshot(&self, tx_id: TxId) {
        let mut s = self.snapshots.lock().expect("snapshot map poisoned");
        if let Some(n) = s.get_mut(&tx_id) {
            *n -= 1;
            if *n == 0 {
                s.remove(&tx_id);
            }
        }
    }

    /// Oldest active snapshot. Used by [`compact`] as the version-
    /// retention floor. Returns `TxId::ACTIVE` when no readers are
    /// registered (= aggressive v1.3 behaviour).
    #[must_use]
    pub fn oldest_active_snapshot(&self) -> TxId {
        let s = self.snapshots.lock().expect("snapshot map poisoned");
        s.keys().next().copied().unwrap_or(TxId::ACTIVE)
    }

    /// Number of distinct snapshot tx_ids currently registered. Test +
    /// monitoring helper.
    #[must_use]
    pub fn active_snapshot_count(&self) -> usize {
        self.snapshots
            .lock()
            .expect("snapshot map poisoned")
            .len()
    }
}

/// Trigger policy for automatic compaction. The default — compact once the
/// live SSTable count reaches 4, checked every 5 s — keeps a write-heavy
/// LSM from accumulating an unbounded fan of tiny flushed tables (which
/// would slowly inflate read amplification) without compacting so eagerly
/// that it wastes I/O on a quiet database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionPolicy {
    /// Compact when the number of live SSTables is `>=` this value.
    pub l0_trigger: usize,
    /// How often the background compactor evaluates the trigger.
    pub check_interval: Duration,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            l0_trigger: 4,
            check_interval: Duration::from_secs(5),
        }
    }
}

/// Handle to a running background compactor. Stops the thread on
/// [`stop`](Self::stop) or when dropped, so the compactor never outlives
/// the handle's owner.
#[derive(Debug)]
pub struct CompactorHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl CompactorHandle {
    /// Signal the background thread to stop and wait for it to finish.
    pub fn stop(mut self) {
        self.signal_and_join();
    }

    fn signal_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for CompactorHandle {
    fn drop(&mut self) {
        self.signal_and_join();
    }
}

/// Run one **off-lock compaction** against `engine`, serialised by
/// `compaction_lock`. The heavy read+merge+write phase runs without holding
/// the engine write lock; only the brief plan + install phases take it.
///
/// `floor` is the snapshot floor to merge against (versions superseded before
/// it may be dropped). `current_floor` is invoked once, under the install
/// lock, to re-read the live floor; if it has regressed below `floor` (a
/// reader registered an older snapshot mid-merge) the run aborts and discards
/// its output rather than drop a version that reader needs.
///
/// Callers with a snapshot registry (e.g. [`SharedEngine`]) pass the oldest
/// active snapshot for both. Callers with **no** registry — where every read
/// is a single lock acquisition, so the atomic install swap is sufficient for
/// consistency — pass [`TxId::ACTIVE`] for both (aggressive drop, no floor to
/// protect). Reuse this from any `RwLock<Engine>` holder (the HTTP server)
/// to get off-lock compaction without adopting the whole `SharedEngine`.
pub fn run_offlock_compaction(
    engine: &RwLock<Engine>,
    compaction_lock: &Mutex<()>,
    floor: TxId,
    current_floor: impl FnOnce() -> TxId,
) -> Result<CompactionStats, EngineError> {
    // Serialise compactions — two concurrent merges could pick overlapping
    // input sets. Does not block ordinary readers/writers.
    let _c = compaction_lock.lock().expect("compaction lock poisoned");

    // Phase 1: plan under the write lock (brief — no data I/O). Capture the
    // live SSTable count in the same lock so a no-op (<2 SSTables) reports it
    // exactly as the locking compaction did.
    let (plan, sstable_count) = {
        let mut e = engine.write().expect("engine lock poisoned");
        let count = e.sstable_count();
        (e.plan_compaction(floor), count)
    };
    let Some(plan) = plan else {
        return Ok(CompactionStats {
            sstables_in: sstable_count,
            ..CompactionStats::default()
        });
    };

    // Phase 2: merge OFF-LOCK. Writers/flushes run concurrently.
    let (records_in, records_out) = match merge_planned(&plan) {
        Ok(v) => v,
        Err(e) => {
            discard_output(&plan);
            return Err(e);
        }
    };

    // Phase 3: install under the write lock, re-checking the floor in the
    // same critical section so no reader can slip between check and swap.
    let mut e = engine.write().expect("engine lock poisoned");
    if current_floor() < plan.floor() {
        drop(e);
        discard_output(&plan);
        return Ok(CompactionStats::default());
    }
    if let Some(stats) = e.install_planned_compaction(&plan, records_in, records_out)? {
        Ok(stats)
    } else {
        // Input set changed (shouldn't happen under the mutex, but be safe).
        drop(e);
        discard_output(&plan);
        Ok(CompactionStats::default())
    }
}

/// Remove an aborted off-lock compaction's output SSTable + the block-index
/// and bloom sidecars its merge wrote (the pidx/vidx/idl sidecars are only
/// written at install, so a pre-install abort never produced them).
fn discard_output(plan: &CompactionPlan) {
    let p = plan.output_path();
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(crate::block_index::sidecar_path_for(p));
    let _ = std::fs::remove_file(crate::bloom::sidecar_path_for(p));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineConfig;
    use crate::record::EntityRecord;
    use std::sync::Arc;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ndb-shared-{name}-{}", uuid::Uuid::now_v7()))
    }

    fn make_entity(name: &str) -> EntityRecord {
        make_entity_with(EntityId::now_v7(), name)
    }

    fn make_entity_with(id: EntityId, name: &str) -> EntityRecord {
        EntityRecord {
            entity_id: id,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(1), Value::String(name.into()))],
        }
    }

    /// Write one entity and flush, producing exactly one new SSTable.
    fn write_one_sstable(eng: &SharedEngine, name: &str) {
        eng.with_write_txn(|mut txn| {
            txn.put_entity(make_entity(name));
            txn.commit()
        })
        .unwrap();
        eng.flush().unwrap();
    }

    #[test]
    fn maybe_compact_respects_the_trigger() {
        let dir = temp_dir("maybe_compact");
        let eng = SharedEngine::create(&dir).unwrap();
        for i in 0..3 {
            write_one_sstable(&eng, &format!("e{i}"));
        }
        assert_eq!(eng.sstable_count(), 3);

        // Trigger not met → no compaction.
        let policy_high = CompactionPolicy {
            l0_trigger: 5,
            check_interval: Duration::from_millis(10),
        };
        assert!(eng.maybe_compact(&policy_high).unwrap().is_none());
        assert_eq!(eng.sstable_count(), 3);

        // Trigger met → compaction collapses the tables to one.
        let policy_low = CompactionPolicy {
            l0_trigger: 2,
            check_interval: Duration::from_millis(10),
        };
        let stats = eng.maybe_compact(&policy_low).unwrap();
        assert!(stats.is_some(), "trigger met → compaction must run");
        assert_eq!(eng.sstable_count(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn offlock_compaction_runs_concurrently_with_writers_without_data_loss() {
        // A writer thread commits + flushes while a compactor thread hammers
        // the off-lock compaction. Because the merge runs without the write
        // lock and the install swaps by SET, writers must never block-fail or
        // lose data, and no commit may go missing.
        let dir = temp_dir("offlock_concurrent");
        let eng = Arc::new(SharedEngine::create(&dir).unwrap());
        let n = 80usize;

        let writer = {
            let e = Arc::clone(&eng);
            std::thread::spawn(move || {
                let mut ids = Vec::with_capacity(n);
                for i in 0..n {
                    let id = EntityId::now_v7();
                    e.with_write_txn(|mut t| {
                        t.put_entity(make_entity_with(id, &format!("e{i}")));
                        t.commit()
                    })
                    .unwrap();
                    ids.push(id);
                    if i % 5 == 4 {
                        e.flush().unwrap();
                    }
                }
                ids
            })
        };
        let compactor = {
            let e = Arc::clone(&eng);
            std::thread::spawn(move || {
                for _ in 0..40 {
                    e.compact_offlock().unwrap();
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let ids = writer.join().unwrap();
        compactor.join().unwrap();
        eng.flush().unwrap();
        eng.compact_offlock().unwrap();

        // Every committed entity must still resolve Live at the latest snapshot.
        let snap = TxId::new(eng.manifest_snapshot().last_tx_id);
        for id in &ids {
            match eng.snapshot_read(&id.into_uuid(), snap).unwrap() {
                Resolved::Live(Record::Entity(e)) => assert_eq!(&e.entity_id, id),
                other => panic!("entity {id:?} lost after concurrent compaction: {other:?}"),
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_stall_plus_auto_flush_with_concurrent_compaction_never_deadlocks() {
        // Auto-flush at 1 byte (every commit flushes → L0 grows fast) and a
        // low stall threshold. A writer drives commits through with_write_txn
        // while a compactor runs off-lock compaction. The whole point: writes
        // are rejected (WriteStalled), never blocked, so there is no deadlock
        // against the compactor's lock-taking install phase — and committed
        // data always survives.
        let dir = temp_dir("stall_concurrent");
        let cfg = EngineConfig {
            memtable_flush_threshold_bytes: 1,
            l0_stall_threshold: 6,
            ..EngineConfig::default()
        };
        let eng = Arc::new(SharedEngine::from_engine(
            Engine::create_with_config(&dir, cfg).unwrap(),
        ));

        let writer = {
            let e = Arc::clone(&eng);
            std::thread::spawn(move || {
                let mut committed = Vec::new();
                let mut stalls = 0u32;
                for i in 0..120 {
                    let id = EntityId::now_v7();
                    let r = e.with_write_txn(|mut t| {
                        t.put_entity(make_entity_with(id, &format!("e{i}")));
                        t.commit()
                    });
                    match r {
                        Ok(_) => committed.push(id),
                        Err(EngineError::WriteStalled { .. }) => {
                            stalls += 1;
                            // Back off WITHOUT holding any lock; the compactor
                            // reduces the SSTable count so a later write lands.
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => panic!("unexpected write error: {e:?}"),
                    }
                }
                (committed, stalls)
            })
        };
        let compactor = {
            let e = Arc::clone(&eng);
            std::thread::spawn(move || {
                for _ in 0..120 {
                    e.compact_offlock().unwrap();
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let (committed, stalls) = writer.join().unwrap();
        compactor.join().unwrap();

        // Backpressure actually engaged, and progress was still made.
        assert!(stalls > 0, "expected some writes to be stalled by backpressure");
        assert!(!committed.is_empty(), "writer should still make progress");

        // Every committed entity survives the storm of flushes + compactions.
        let snap = TxId::new(eng.manifest_snapshot().last_tx_id);
        for id in &committed {
            match eng.snapshot_read(&id.into_uuid(), snap).unwrap() {
                Resolved::Live(Record::Entity(e)) => assert_eq!(&e.entity_id, id),
                other => panic!("committed entity {id:?} lost: {other:?}"),
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_compactor_compacts_in_background_then_stops() {
        let dir = temp_dir("auto_compact");
        let eng = Arc::new(SharedEngine::create(&dir).unwrap());
        for i in 0..3 {
            write_one_sstable(&eng, &format!("e{i}"));
        }
        assert_eq!(eng.sstable_count(), 3);

        let handle = SharedEngine::spawn_auto_compactor(
            &eng,
            CompactionPolicy {
                l0_trigger: 2,
                check_interval: Duration::from_millis(20),
            },
        );

        // Poll for the background compaction to land (bounded wait).
        let mut compacted = false;
        for _ in 0..200 {
            if eng.sstable_count() <= 1 {
                compacted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        handle.stop();
        assert!(compacted, "auto-compactor should have collapsed the SSTables");
        assert_eq!(eng.sstable_count(), 1);

        // Data survives the background compaction.
        let snap = TxId::new(eng.manifest_snapshot().last_tx_id);
        assert!(snap.get() >= 3);

        std::fs::remove_dir_all(&dir).unwrap();
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
    fn snapshot_aware_compaction_protects_registered_reader() {
        let dir = temp_dir("snap-protect");
        let eng = SharedEngine::create(&dir).unwrap();
        // 3 versions of the same entity across 3 SSTables.
        let eid = EntityId::now_v7();
        let mut versions = Vec::new();
        for v in 1..=3 {
            let tx = eng
                .with_write_txn(|mut txn| {
                    txn.put_entity(EntityRecord {
                        entity_id: eid,
                        type_id: TypeId::new(1),
                        tx_id_assert: TxId::new(0),
                        tx_id_supersede: TxId::ACTIVE,
                        properties: vec![(PropertyId::new(1), Value::I64(v))],
                    });
                    txn.commit()
                })
                .unwrap();
            versions.push(tx);
            eng.flush().unwrap();
        }
        // Pin a reader at the SECOND commit's tx_id.
        eng.register_snapshot(versions[1]);
        assert_eq!(eng.active_snapshot_count(), 1);
        assert_eq!(eng.oldest_active_snapshot(), versions[1]);
        // Compact — must keep v2 and v3 (v1 dropped: its supersession
        // tx == versions[1] which is the floor, fully shadowed).
        eng.compact().unwrap();
        let entities = count_entities(&dir);
        assert_eq!(entities, 2, "active reader at v2 preserves v2 and v3");
        // Release; next compaction is fully aggressive.
        eng.release_snapshot(versions[1]);
        assert_eq!(eng.active_snapshot_count(), 0);
        // Add a 4th version to recreate compactable history.
        eng.with_write_txn(|mut txn| {
            txn.put_entity(EntityRecord {
                entity_id: eid,
                type_id: TypeId::new(1),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![(PropertyId::new(1), Value::I64(99))],
            });
            txn.commit()
        })
        .unwrap();
        eng.flush().unwrap();
        eng.compact().unwrap();
        let entities = count_entities(&dir);
        assert_eq!(entities, 1, "no active reader → drop everything but latest");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn count_entities(dir: &std::path::Path) -> usize {
        let mut n = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_some_and(|e| e == "ndb") {
                let r = crate::sstable::SSTableReader::open(&p).unwrap();
                for item in r.iter() {
                    let (rec, _) = item.unwrap();
                    if matches!(rec, Record::Entity(_)) {
                        n += 1;
                    }
                }
            }
        }
        n
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

    /// Acceptance test for the v3-final RwLock migration. Spawns 16
    /// reader threads doing 10k `snapshot_read`s each against the same
    /// `Arc<RwLock<Engine>>` and asserts the wall time is meaningfully
    /// below the single-thread baseline. With the previous `Mutex`,
    /// concurrent throughput was ≈ single-thread (workers queue on the
    /// lock); RwLock readers parallelise.
    ///
    /// We assert `parallel_wall < 0.5 * baseline_wall` — a conservative
    /// bound that holds on every modern multi-core box including CI
    /// runners. Mathematically a perfectly-parallel implementation
    /// would clear ≈ baseline/min(16, n_cpus); 0.5 is the floor we use
    /// to avoid flaking on 2-vCPU CI hardware while still rejecting any
    /// regression that puts the work back behind a single lock.
    ///
    /// Bumping the assertion to `< baseline / 8` (the original spec
    /// target) is the right move once we have dedicated 16-core CI;
    /// for now `< 0.5` is a hard "actually parallel" signal.
    #[test]
    fn concurrent_point_lookups_scale() {
        let dir = temp_dir("concurrent-reads");
        let eng = Arc::new(SharedEngine::create(&dir).unwrap());

        // Seed: 1000 entities, capture the UUIDs we'll probe.
        let mut probe_uuids: Vec<uuid::Uuid> = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let eid = EntityId::now_v7();
            probe_uuids.push(eid.into_uuid());
            eng.with_write_txn(|mut txn| {
                txn.put_entity(EntityRecord {
                    entity_id: eid,
                    type_id: TypeId::new(1),
                    tx_id_assert: TxId::new(0),
                    tx_id_supersede: TxId::ACTIVE,
                    properties: vec![(PropertyId::new(1), Value::String("x".into()))],
                });
                txn.commit()
            })
            .unwrap();
        }
        eng.flush().unwrap();
        let snap = TxId::new(eng.manifest_snapshot().last_tx_id);

        // Baseline: single thread does 16 × 10k = 160k lookups serially.
        const N_THREADS: usize = 16;
        const N_PER_THREAD: usize = 10_000;
        let total = N_THREADS * N_PER_THREAD;
        let start = std::time::Instant::now();
        for i in 0..total {
            let u = probe_uuids[i % probe_uuids.len()];
            let _ = eng.snapshot_read(&u, snap).unwrap();
        }
        let baseline_ns = start.elapsed().as_nanos();

        // Concurrent: 16 threads × 10k lookups each, same total work.
        let probe_uuids = Arc::new(probe_uuids);
        let start = std::time::Instant::now();
        let handles: Vec<_> = (0..N_THREADS)
            .map(|tid| {
                let e = Arc::clone(&eng);
                let uuids = Arc::clone(&probe_uuids);
                std::thread::spawn(move || {
                    for i in 0..N_PER_THREAD {
                        let idx = (tid * 31 + i) % uuids.len();
                        let _ = e.snapshot_read(&uuids[idx], snap).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let parallel_ns = start.elapsed().as_nanos();

        // Speedup assertion: parallel must be meaningfully faster than
        // serial. On 2-core CI we expect ~1.5–1.8×; on a desktop box
        // 6–12×. 2× is the floor that rejects "workers queued on a
        // single lock" while staying robust across hardware.
        let speedup = baseline_ns as f64 / parallel_ns.max(1) as f64;
        assert!(
            speedup >= 2.0,
            "RwLock<Engine> readers did not parallelise: baseline {} ns, parallel {} ns, speedup {:.2}×",
            baseline_ns,
            parallel_ns,
            speedup,
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
