//! High-arity N-ary storage probe: build E entities + N facts of arity K
//! (each fact links K entities by distinct roles) into a fresh nDB and print
//! the directory so on-disk size can be measured. Sweep K via the ARITY env.
//!
//!     ARITY=4 NFACTS=50000 NENTS=10000 cargo run --release --example arity_storage
//!
//! Compared against the standard relational modelling of variable-arity
//! relationships — a `fact_role(fact_id, role, entity_id)` association table,
//! which costs N*K rows where nDB costs N records.

use ndb_engine::{
    Engine, EntityId, EntityRecord, HyperEdgeRecord, HyperedgeId, RoleId, TxId, TypeId,
};

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let k = env_usize("ARITY", 3);
    let n = env_usize("NFACTS", 50_000);
    let e = env_usize("NENTS", 10_000);
    let dir = std::env::temp_dir().join(format!("ndb-arity-k{k}-{}", uuid::Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir)?;
    let mut engine = Engine::create(&dir)?;

    let mut ents: Vec<EntityId> = Vec::with_capacity(e);
    let mut tx = engine.begin_write();
    let mut c = 0;
    for _ in 0..e {
        let id = EntityId::now_v7();
        ents.push(id);
        tx.put_entity(EntityRecord {
            entity_id: id,
            type_id: TypeId::new(1),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![],
        });
        c += 1;
        if c >= 1000 {
            tx.commit()?;
            tx = engine.begin_write();
            c = 0;
        }
    }
    tx.commit()?;

    let mut tx = engine.begin_write();
    c = 0;
    for i in 0..n {
        let roles: Vec<(RoleId, EntityId)> = (0..k)
            .map(|r| (RoleId::new(10 + r as u32), ents[(i * 7 + r * 13) % e]))
            .collect();
        tx.put_hyperedge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(200),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            roles,
            hyperedge_roles: vec![],
            properties: vec![],
        });
        c += 1;
        if c >= 1000 {
            tx.commit()?;
            tx = engine.begin_write();
            c = 0;
        }
    }
    tx.commit()?;

    println!("DIR={}", dir.display());
    println!("k={k} n={n} e={e}");
    Ok(())
}
