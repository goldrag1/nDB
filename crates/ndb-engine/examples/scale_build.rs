//! Build + measure a large low-RAM nDB to validate Option B at scale
//! (the ~10 GB RSS test). Langgraph-shaped: each paper is an entity with a
//! name (lookup-key), citations (property B-tree), and a 128-d embedding
//! (vector); consecutive papers are linked by a CITES hyperedge. All six
//! secondary indexes are registered, so under `low_memory` every one is
//! served from an on-disk sidecar.
//!
//! Build:   cargo run --release --example scale_build -- build <dir> <target_gb>
//! Measure: cargo run --release --example scale_build -- measure <dir>
//!
//! Measure opens the DB in a FRESH process under `low_memory(2 GiB)`,
//! prints process RSS + per-index resident estimate, then runs a few
//! queries (top-K, kNN, by-type, neighbours) and prints RSS again — so the
//! reported RSS reflects only the opened engine + query working set, not
//! the build-time buffers.

use std::path::{Path, PathBuf};
use std::time::Instant;

use ndb_engine::record::{EntityRecord, HyperEdgeRecord};
use ndb_engine::value::Value;
use ndb_engine::{
    Distance, Engine, EngineConfig, EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId,
};

const TYPE_PAPER: u32 = 1;
const TYPE_CITES: u32 = 100;
const PROP_NAME: u32 = 1;
const PROP_CITES: u32 = 2;
const PROP_EMBED: u32 = 3;
const DIM: usize = 128;
const CACHE: usize = 2 * 1024 * 1024 * 1024; // 2 GiB budget

fn embed(seed: u64) -> Vec<f32> {
    (0..DIM)
        .map(|i| {
            let x = seed
                .wrapping_mul(2_654_435_761)
                .wrapping_add(i as u64 * 40_503)
                % 2_000;
            (x as f32) / 1000.0 - 1.0
        })
        .collect()
}

fn rss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest
                .split_whitespace()
                .next()
                .and_then(|x| x.parse().ok())
                .unwrap_or(0.0);
            return kb / 1024.0;
        }
    }
    0.0
}

fn dir_size_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

fn register(engine: &mut Engine) {
    engine.register_lookup_key(PropertyId::new(PROP_NAME));
    engine.register_property_btree(TypeId::new(TYPE_PAPER), PropertyId::new(PROP_CITES));
    engine.register_vector_property(PropertyId::new(PROP_EMBED));
}

fn build(dir: &Path, target_gb: f64) {
    let target = (target_gb * 1024.0 * 1024.0 * 1024.0) as u64;
    let mut engine = Engine::create_with_config(dir, EngineConfig::low_memory(CACHE)).unwrap();
    register(&mut engine);
    let t0 = Instant::now();
    let mut prev: Option<EntityId> = None;
    let mut n: u64 = 0;
    const BATCH: u64 = 5_000;
    const FLUSH_EVERY: u64 = 250_000;
    let mut since_flush: u64 = 0;
    loop {
        let mut tx = engine.begin_write();
        for _ in 0..BATCH {
            let id = EntityId::now_v7();
            tx.put_entity(EntityRecord {
                entity_id: id,
                type_id: TypeId::new(TYPE_PAPER),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![
                    (
                        PropertyId::new(PROP_NAME),
                        Value::String(format!("paper-{n}")),
                    ),
                    (
                        PropertyId::new(PROP_CITES),
                        Value::I64((n as i64 * 2_654_435_761) % 5_000_000),
                    ),
                    (PropertyId::new(PROP_EMBED), Value::Vector(embed(n))),
                ],
            });
            if let Some(p) = prev {
                tx.put_hyperedge(HyperEdgeRecord {
                    hyperedge_id: HyperedgeId::now_v7(),
                    type_id: TypeId::new(TYPE_CITES),
                    tx_id_assert: TxId::new(0),
                    tx_id_supersede: TxId::ACTIVE,
                    roles: vec![(RoleId::new(1), id), (RoleId::new(2), p)],
                    hyperedge_roles: vec![],
                    properties: vec![],
                });
            }
            prev = Some(id);
            n += 1;
            since_flush += 1;
        }
        tx.commit().unwrap();
        if since_flush >= FLUSH_EVERY {
            engine.flush().unwrap();
            since_flush = 0;
            let sz = dir_size_bytes(dir);
            println!(
                "  {n:>10} papers | {:.2} GB on disk | RSS {:.0} MB | {:.0}s",
                sz as f64 / 1.073_741_824e9,
                rss_mb(),
                t0.elapsed().as_secs_f64()
            );
            if sz >= target {
                break;
            }
        }
    }
    engine.flush().unwrap();
    engine.close().unwrap();
    let sz = dir_size_bytes(dir);
    println!(
        "BUILD DONE: {n} papers, {:.2} GB on disk, {:.0}s",
        sz as f64 / 1.073_741_824e9,
        t0.elapsed().as_secs_f64()
    );
}

fn measure(dir: &Path) {
    let sz = dir_size_bytes(dir);
    println!("DB on disk: {:.2} GB", sz as f64 / 1.073_741_824e9);
    let rss_pre = rss_mb();
    let t_open = Instant::now();
    let mut engine = Engine::open_with_config(dir, EngineConfig::low_memory(CACHE)).unwrap();
    register(&mut engine);
    engine.rebuild_indexes().unwrap();
    let open_s = t_open.elapsed().as_secs_f64();
    let rss_open = rss_mb();
    let s = engine.index_memory_stats();
    let mb = |b: usize| b as f64 / 1_048_576.0;
    println!("open: {open_s:.1}s | RSS {rss_pre:.0} -> {rss_open:.0} MB");
    println!(
        "  index resident est: {:.1} MB [lk {:.1} adj {:.1} tc {:.1} etc {:.1} vec {:.1} pbt {:.1}] memtable {:.1}",
        mb(s.index_total()),
        mb(s.lookup_key),
        mb(s.adjacency),
        mb(s.type_cluster),
        mb(s.entity_type_cluster),
        mb(s.vector),
        mb(s.property_btree),
        mb(s.memtable),
    );

    let paper = TypeId::new(TYPE_PAPER);
    let cites = PropertyId::new(PROP_CITES);
    let embed_p = PropertyId::new(PROP_EMBED);

    // --- BOUNDED queries (the tile-server workload) — RSS stays low. ---
    let t = Instant::now();
    let top = engine.property_top_k(paper, cites, 10);
    println!(
        "  [bounded] property_top_k(10): {} hits in {:.1} ms",
        top.len(),
        t.elapsed().as_secs_f64() * 1e3
    );

    if let Some(first) = top.first() {
        let t = Instant::now();
        let nb = engine.hyperedges_for_entity(*first);
        println!(
            "  [bounded] hyperedges_for_entity: {} in {:.1} ms",
            nb.len(),
            t.elapsed().as_secs_f64() * 1e3
        );
    }

    let t = Instant::now();
    let hit = engine.lookup_by_external_key(
        PropertyId::new(PROP_NAME),
        &Value::String("paper-100000".into()),
    );
    println!(
        "  [bounded] lookup_by_external_key: {} in {:.1} ms",
        hit.is_some(),
        t.elapsed().as_secs_f64() * 1e3
    );
    println!(
        "RSS after bounded queries: {:.0} MB  ← the held-low figure",
        rss_mb()
    );

    // --- FULL-SCAN ops: inherently O(N). Brute-force kNN reads every
    //     embedding; verified count gathers every id. mmap keeps these
    //     pages reclaimable, but RSS spikes toward the working set during
    //     them. (HNSW + maintained counts would bound these — future work.)
    let t = Instant::now();
    let knn = engine.vector_search(embed_p, &embed(12_345), 10, Distance::L2Squared);
    println!(
        "  [O(N) scan] vector_search(k=10): {} hits in {:.1} ms (brute-force reads every embedding)",
        knn.len(),
        t.elapsed().as_secs_f64() * 1e3
    );
    println!(
        "RSS after kNN scan: {:.0} MB (mmap-reclaimable; HNSW would bound this)",
        rss_mb()
    );
    engine.close().unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let dir = PathBuf::from(
        args.get(1)
            .cloned()
            .unwrap_or_else(|| "/tmp/ndb-scale".into()),
    );
    match cmd {
        "build" => {
            let gb: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10.0);
            build(&dir, gb);
        }
        "measure" => measure(&dir),
        _ => eprintln!("usage: scale_build <build|measure> <dir> [target_gb]"),
    }
}
