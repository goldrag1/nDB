//! Vector kNN probe: N entities each carrying a D-dim embedding; build the
//! HNSW snapshot and time top-k similarity search. nDB does this natively;
//! SQLite/MariaDB have no vector index, so their only option is a brute-force
//! O(N·D) scan (measured separately in numpy as a generous floor).
//!
//!     NVEC=50000 DIM=128 cargo run --release --example vector_bench

use std::time::Instant;

use ndb_engine::index::Distance;
use ndb_engine::{Engine, EntityId, EntityRecord, PropertyId, TxId, TypeId, Value};

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d)
}

// deterministic pseudo-random vector
fn vec_for(seed: u64, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|j| {
            let x = seed.wrapping_mul(6364136223846793005).wrapping_add(j as u64 * 977 + 1);
            ((x >> 33) as f32 / u32::MAX as f32) - 0.5
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let n = env_usize("NVEC", 50_000);
    let dim = env_usize("DIM", 128);
    let pid = PropertyId::new(30);
    let dir = std::env::temp_dir().join(format!("ndb-vec-{}", uuid::Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir)?;
    let mut engine = Engine::create(&dir)?;
    engine.register_vector_property(pid);

    let mut tx = engine.begin_write();
    let mut c = 0;
    for i in 0..n {
        tx.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![(pid, Value::Vector(vec_for(i as u64, dim)))],
        });
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

    let t = Instant::now();
    let indexed = engine.build_vector_snapshot(pid)?;
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;

    let queries: Vec<Vec<f32>> = (0..1000).map(|q| vec_for(1_000_000 + q as u64, dim)).collect();
    let t = Instant::now();
    let mut hits = 0usize;
    for q in &queries {
        if let Some(r) = engine.vector_search_snapshot(pid, q, 10, Distance::Cosine) {
            hits += r.len();
        }
    }
    let knn_us = t.elapsed().as_micros() as f64 / queries.len() as f64;

    println!("DIR={}", dir.display());
    println!(
        "n={n} dim={dim} indexed={indexed} build_ms={build_ms:.0} knn_us={knn_us:.2} avg_hits={}",
        hits / queries.len()
    );
    Ok(())
}
