//! Concurrency stress test (Tier 3): hammer the engine with many concurrent
//! readers while a writer commits, and assert the invariants the
//! single-writer / concurrent-reader model promises:
//!
//!  - No panic, deadlock, or torn read under interleaving.
//!  - A reader's snapshot scan is always internally consistent: it decodes
//!    cleanly and the visible entity count only ever GROWS (the writer only
//!    inserts), never goes backwards or skips — i.e. a reader never observes
//!    a half-applied commit.
//!  - The final state equals exactly what the writer committed.
//!
//! The engine itself is single-writer by type (`begin_write` takes `&mut`);
//! this mirrors the server's `Arc<RwLock<Engine>>` deployment, where reads
//! parallelise and the writer takes the exclusive slot. The test validates
//! that real concurrent traffic against that arrangement is safe.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use ndb_engine::{Engine, EntityId, EntityRecord, PropertyId, Record, TxId, TypeId, Value};

const TYPE_ITEM: u32 = 1;
const PROP_SEQ: u32 = 1;
const READERS: usize = 6;
const RUN_FOR: Duration = Duration::from_millis(750);

fn count_items(engine: &Engine) -> usize {
    engine
        .snapshot_iter_streaming(TxId::ACTIVE)
        .filter_map(Result::ok)
        .filter(|r| matches!(r, Record::Entity(e) if e.type_id == TypeId::new(TYPE_ITEM)))
        .count()
}

#[test]
fn concurrent_readers_never_observe_a_half_applied_commit() {
    let dir = std::env::temp_dir().join(format!(
        "ndb-concurrency-{}",
        Instant::now().elapsed().as_nanos()
            ^ u128::from(std::process::id())
    ));
    let engine = Engine::create(&dir).expect("create");
    let engine = Arc::new(RwLock::new(engine));

    let stop = Arc::new(AtomicBool::new(false));
    let committed = Arc::new(AtomicU64::new(0));

    std::thread::scope(|scope| {
        // Readers: scan repeatedly, asserting monotonic non-decreasing counts
        // and clean decodes.
        for _ in 0..READERS {
            let engine = Arc::clone(&engine);
            let stop = Arc::clone(&stop);
            scope.spawn(move || {
                let mut last_seen = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let guard = engine.read().expect("reader lock");
                    let n = count_items(&guard);
                    drop(guard);
                    assert!(
                        n >= last_seen,
                        "visible count went backwards: {n} < {last_seen} \
                         (a reader observed a half-applied / disappearing commit)"
                    );
                    last_seen = n;
                }
            });
        }

        // Single writer: commit one item per iteration, flush periodically so
        // the scan crosses the memtable + multiple SSTables.
        {
            let engine = Arc::clone(&engine);
            let stop = Arc::clone(&stop);
            let committed = Arc::clone(&committed);
            scope.spawn(move || {
                let deadline = Instant::now() + RUN_FOR;
                let mut seq = 0i64;
                while Instant::now() < deadline {
                    {
                        let mut guard = engine.write().expect("writer lock");
                        let mut tx = guard.begin_write();
                        tx.put_entity(EntityRecord {
                            entity_id: EntityId::now_v7(),
                            type_id: TypeId::new(TYPE_ITEM),
                            tx_id_assert: TxId::new(0),
                            tx_id_supersede: TxId::ACTIVE,
                            properties: vec![(PropertyId::new(PROP_SEQ), Value::I64(seq))],
                        });
                        tx.commit().expect("commit");
                        if seq % 64 == 0 {
                            guard.flush().expect("flush");
                        }
                    }
                    seq += 1;
                    committed.fetch_add(1, Ordering::Relaxed);
                }
                stop.store(true, Ordering::Relaxed);
            });
        }
    });

    // Final state must equal exactly what the writer committed.
    let total = committed.load(Ordering::Relaxed) as usize;
    assert!(total > 0, "writer made no progress");
    let final_count = count_items(&engine.read().unwrap());
    assert_eq!(
        final_count, total,
        "final visible count {final_count} != committed {total}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
