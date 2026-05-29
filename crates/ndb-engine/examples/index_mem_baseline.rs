//! Baseline RAM-vs-DB-size curve for the low-RAM core work (Option B,
//! Phase 0). Ingests a langgraph-shaped corpus (entities with a numeric
//! B-tree property, a 16-d vector embedding, a lookup-key name, plus
//! CITES hyperedges feeding the adjacency + type-cluster indexes) at
//! several scales, flushes, REOPENS (triggering `rebuild_indexes` — the
//! RAM hog), then prints per-index resident estimates + process RSS.
//!
//! Run: `cargo run --release --example index_mem_baseline [scales...]`
//! e.g. `cargo run --release --example index_mem_baseline 25000 50000 100000`

use std::path::PathBuf;

use ndb_engine::record::{EntityRecord, HyperEdgeRecord};
use ndb_engine::value::Value;
use ndb_engine::{
    Engine, EngineConfig, EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId,
};

const TYPE_PAPER: u32 = 1;
const PROP_NAME: u32 = 1; // lookup key
const PROP_CITES: u32 = 2; // property b-tree
const PROP_EMBED: u32 = 3; // vector (16-d)
const TYPE_CITES: u32 = 100; // hyperedge type
const ROLE_SRC: u32 = 1;
const ROLE_DST: u32 = 2;
const DIM: usize = 16;

fn embed(seed: u64) -> Vec<f32> {
    // Cheap deterministic pseudo-embedding.
    (0..DIM)
        .map(|i| {
            let x = (seed.wrapping_mul(2_654_435_761).wrapping_add(i as u64)) % 1000;
            x as f32 / 1000.0
        })
        .collect()
}

fn rss_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            return kb / 1024.0;
        }
    }
    0.0
}

fn build_and_measure(n: usize) {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "ndb-mem-baseline-{}-{}",
        n,
        uuid::Uuid::now_v7().simple()
    ));
    {
        let mut engine = Engine::create(&dir).unwrap();
        engine.register_lookup_key(PropertyId::new(PROP_NAME));
        engine.register_property_btree(TypeId::new(TYPE_PAPER), PropertyId::new(PROP_CITES));
        engine.register_vector_property(PropertyId::new(PROP_EMBED));

        let mut ids: Vec<EntityId> = Vec::with_capacity(n);
        // Batch ~2000 entities (+ their CITES edges) per transaction so the
        // commit/fsync count stays low; flush periodically to bound ingest
        // RAM. Index sizes are identical to per-row commits.
        const BATCH: usize = 2000;
        let mut i = 0;
        while i < n {
            let mut txn = engine.begin_write();
            let end = (i + BATCH).min(n);
            for j in i..end {
                let eid = EntityId::now_v7();
                ids.push(eid);
                txn.put_entity(EntityRecord {
                    entity_id: eid,
                    type_id: TypeId::new(TYPE_PAPER),
                    tx_id_assert: TxId::new(0),
                    tx_id_supersede: TxId::ACTIVE,
                    properties: vec![
                        (PropertyId::new(PROP_NAME), Value::String(format!("paper-{j}"))),
                        (PropertyId::new(PROP_CITES), Value::I64((j as i64 * 7) % 100_000)),
                        (PropertyId::new(PROP_EMBED), Value::Vector(embed(j as u64))),
                    ],
                });
                if j > 0 {
                    txn.put_hyperedge(HyperEdgeRecord {
                        hyperedge_id: HyperedgeId::now_v7(),
                        type_id: TypeId::new(TYPE_CITES),
                        tx_id_assert: TxId::new(0),
                        tx_id_supersede: TxId::ACTIVE,
                        roles: vec![
                            (RoleId::new(ROLE_SRC), ids[j]),
                            (RoleId::new(ROLE_DST), ids[j - 1]),
                        ],
                        hyperedge_roles: vec![],
                        properties: vec![],
                    });
                }
            }
            txn.commit().unwrap();
            engine.flush().unwrap();
            i = end;
        }
        engine.close().unwrap();
    }

    let mb = |b: usize| b as f64 / 1_048_576.0;

    // (a) Default open → full RAM rebuild (the hog under measurement).
    let s_def = {
        let engine = Engine::open(&dir).unwrap();
        let s = engine.index_memory_stats();
        println!(
            "N={n:>7} default   | indexes est {:7.1} MB \
             [lk {:.1} adj {:.1} tc {:.1} etc {:.1} vec {:.1} pbt {:.1}]",
            mb(s.index_total()),
            mb(s.lookup_key),
            mb(s.adjacency),
            mb(s.type_cluster),
            mb(s.entity_type_cluster),
            mb(s.vector),
            mb(s.property_btree),
        );
        engine
            .index_memory_stats()
            .property_btree
            .max(s.property_btree)
    };

    // (b) Low-memory open → property B-tree served from .pidx sidecars
    //     (Phase 1c). Register + rebuild like the langgraph server does.
    {
        let mut engine =
            Engine::open_with_config(&dir, EngineConfig::low_memory(2 * 1024 * 1024 * 1024)).unwrap();
        engine.register_property_btree(TypeId::new(TYPE_PAPER), PropertyId::new(PROP_CITES));
        engine.register_vector_property(PropertyId::new(PROP_EMBED));
        engine.rebuild_indexes().unwrap();
        let s = engine.index_memory_stats();
        println!(
            "N={n:>7} lowmemory | indexes est {:7.1} MB \
             [lk {:.1} adj {:.1} tc {:.1} etc {:.1} vec {:.1} pbt {:.1}]  \
             ← property index now on disk (pbt {:.1}->{:.1} MB)",
            mb(s.index_total()),
            mb(s.lookup_key),
            mb(s.adjacency),
            mb(s.type_cluster),
            mb(s.entity_type_cluster),
            mb(s.vector),
            mb(s.property_btree),
            mb(s_def),
            mb(s.property_btree),
        );
        drop(engine);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let scales: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let scales = if scales.is_empty() {
        vec![25_000, 50_000, 100_000]
    } else {
        scales
    };
    println!("== index RAM baseline (Option B Phase 0) ==");
    for n in scales {
        build_and_measure(n);
    }
}
