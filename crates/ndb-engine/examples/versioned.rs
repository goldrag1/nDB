//! Time-travel probe: N entities each updated M times (M MVCC versions), then
//! measure (a) on-disk size of all retained versions and (b) latency of an
//! "as-of" read at the first version vs a read at the latest — nDB does both as
//! native snapshot reads.
//!
//!     NENTS=10000 VERSIONS=10 cargo run --release --example versioned

use std::time::Instant;

use ndb_engine::{Engine, EntityId, EntityRecord, PropertyId, TxId, TypeId, Value};

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let n = env_usize("NENTS", 10_000);
    let m = env_usize("VERSIONS", 10);
    let dir =
        std::env::temp_dir().join(format!("ndb-versioned-{}", uuid::Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir)?;
    let mut engine = Engine::create(&dir)?;

    let mut ids: Vec<EntityId> = Vec::with_capacity(n);
    let put = |tx: &mut ndb_engine::WriteTxn, id: EntityId, v: i64| {
        tx.put_entity(EntityRecord {
            entity_id: id,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(30), Value::I64(v))],
        });
    };

    // round 0
    let mut tx = engine.begin_write();
    let mut c = 0;
    let mut last = TxId::new(0);
    for _ in 0..n {
        let id = EntityId::now_v7();
        ids.push(id);
        put(&mut tx, id, 0);
        c += 1;
        if c >= 1000 {
            last = tx.commit()?;
            tx = engine.begin_write();
            c = 0;
        }
    }
    if c > 0 {
        last = tx.commit()?;
    }
    let early_tx = last; // snapshot containing the round-0 values

    // rounds 1..M — each rewrites every entity's property (a new version)
    for round in 1..m {
        let mut tx = engine.begin_write();
        let mut c = 0;
        for &id in &ids {
            put(&mut tx, id, round as i64);
            c += 1;
            if c >= 1000 {
                tx.commit()?;
                tx = engine.begin_write();
                c = 0;
            }
        }
        if c > 0 {
            tx.commit()?;
        }
    }

    // as-of vs now read latency
    let sample: Vec<EntityId> = (0..2000).map(|i| ids[(i * 7) % n]).collect();
    let t = Instant::now();
    for &id in &sample {
        let _ = engine.snapshot_read(&id.into_uuid(), early_tx);
    }
    let asof = t.elapsed().as_micros() as f64 / sample.len() as f64;
    let t = Instant::now();
    for &id in &sample {
        let _ = engine.snapshot_read(&id.into_uuid(), TxId::ACTIVE);
    }
    let now = t.elapsed().as_micros() as f64 / sample.len() as f64;

    println!("DIR={}", dir.display());
    println!("n={n} versions={m} asof_us={asof:.2} now_us={now:.2}");
    Ok(())
}
