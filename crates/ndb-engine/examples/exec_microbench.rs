//! Single-threaded A/B microbench for the query executor hot paths.
//!
//! Seeds the SAME dataset shape as `examples/bench_race.rs`
//! (49k customers across 1k regions ≈ 49/region, 45k hub-routed sales
//! hyperedges) then runs `single_pattern_query` and `two_pattern_join`
//! in tight single-threaded loops, reporting median / p99 µs per op.
//!
//! This is the deterministic A/B harness for executor optimizations:
//! build it on the "before" commit, record numbers, apply the change,
//! rebuild, compare. Source is identical across both builds so the only
//! variable is the engine.
//!
//! Run with:
//!     cargo run --release --example exec_microbench
//!     cargo run --release --example exec_microbench -- 2000   # iters override
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use ndb_engine::record::Record;
use ndb_engine::wire::JsonValue;
use ndb_engine::wire_query::{
    CmpOp, Pattern, PropertyFilter, QueryRequest, ReturnItem, RoleBinding, Term,
};
use ndb_engine::{
    Engine, EntityId, EntityRecord, HyperEdgeRecord, HyperedgeId, PropertyId, RoleId, TxId, TypeId,
    Value,
};
use std::time::Instant;

const TYPE_CUSTOMER: u32 = 100;
const TYPE_REGION: u32 = 101;
const TYPE_SALES: u32 = 200;
const PROP_NAME: u32 = 30;
const PROP_REGION: u32 = 31;
const PROP_CODE: u32 = 32;
const ROLE_BUYER: u32 = 10;

const N_CUSTOMERS: usize = 49_000;
const N_REGIONS: usize = 1_000;
const N_SALES_ORDERS: usize = 45_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let iters: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);

    let dir = std::env::temp_dir().join(format!(
        "ndb-exec-microbench-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir)?;

    let mut engine = Engine::create(&dir)?;
    engine.register_property_btree(TypeId::new(TYPE_CUSTOMER), PropertyId::new(PROP_REGION));
    register_dictionaries(&mut engine);

    let load_start = Instant::now();
    let region_codes = load_regions(&mut engine);
    let customer_ids = load_customers(&mut engine, &region_codes);
    let _sales = load_sales(&mut engine, &customer_ids);
    eprintln!(
        "seeded {} customers + {} regions + {} sales in {:.0} ms ({} sstables)",
        customer_ids.len(),
        region_codes.len(),
        N_SALES_ORDERS,
        load_start.elapsed().as_secs_f64() * 1000.0,
        engine.sstable_count(),
    );

    let narrow_region = region_codes[0].clone();

    // Warmup — touch both paths so caches/pages are hot.
    for _ in 0..20 {
        let _ = ndb_engine::query::execute_read(&engine, single_pattern_request(&narrow_region));
        let _ = ndb_engine::query::execute_read(&engine, two_pattern_request(&narrow_region));
    }

    bench("single_pattern_query", iters, || {
        let r = ndb_engine::query::execute_read(&engine, single_pattern_request(&narrow_region))
            .unwrap();
        r.rows.len()
    });

    bench("two_pattern_join", iters, || {
        let r =
            ndb_engine::query::execute_read(&engine, two_pattern_request(&narrow_region)).unwrap();
        r.rows.len()
    });

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Run `op` `iters` times, print median / p99 µs and the row count
/// produced (sanity check that the workload did real work).
fn bench<F: FnMut() -> usize>(name: &str, iters: usize, mut op: F) {
    let mut samples = Vec::with_capacity(iters);
    let mut rows = 0;
    let outer = Instant::now();
    for _ in 0..iters {
        let t = Instant::now();
        rows = op();
        samples.push(t.elapsed().as_nanos() as u64);
    }
    let wall = outer.elapsed().as_secs_f64();
    samples.sort_unstable();
    let n = samples.len();
    let p50 = samples[n / 2] as f64 / 1000.0;
    let p99 = samples[(n * 99 / 100).min(n - 1)] as f64 / 1000.0;
    let min = samples[0] as f64 / 1000.0;
    let ops = n as f64 / wall;
    println!(
        "{name:<22} rows={rows:<4} min={min:7.2}µs  p50={p50:7.2}µs  p99={p99:7.2}µs  {ops:>10.0} ops/s (1 thread)"
    );
}

fn single_pattern_request(region: &str) -> QueryRequest {
    QueryRequest {
        as_of: None,
        patterns: vec![Pattern::Entity {
            type_id: TYPE_CUSTOMER,
            self_var: Some("c".into()),
            property_filters: vec![PropertyFilter {
                property_id: PROP_REGION,
                op: CmpOp::Eq,
                term: Term::Literal {
                    value: JsonValue::String {
                        value: region.into(),
                    },
                },
            }],
        }],
        filter: None,
        returns: vec![ReturnItem::from("c")],
        order_by: vec![],
        limit: None,
        creates: vec![],
        deletes: vec![],
        sets: vec![],
        merges: vec![],
    }
}

fn two_pattern_request(region: &str) -> QueryRequest {
    QueryRequest {
        as_of: None,
        patterns: vec![
            Pattern::Entity {
                type_id: TYPE_CUSTOMER,
                self_var: Some("c".into()),
                property_filters: vec![PropertyFilter {
                    property_id: PROP_REGION,
                    op: CmpOp::Eq,
                    term: Term::Literal {
                        value: JsonValue::String {
                            value: region.into(),
                        },
                    },
                }],
            },
            Pattern::Hyperedge {
                type_id: TYPE_SALES,
                self_var: None,
                role_bindings: vec![RoleBinding {
                    role_id: ROLE_BUYER,
                    term: Term::Var { name: "c".into() },
                }],
                property_filters: vec![],
                recursion: None,
            },
        ],
        filter: None,
        returns: vec![ReturnItem::from("c")],
        order_by: vec![],
        limit: None,
        creates: vec![],
        deletes: vec![],
        sets: vec![],
        merges: vec![],
    }
}

// ─── Data load (mirror of bench_race.rs) ───────────────────────────────

fn register_dictionaries(engine: &mut Engine) {
    use ndb_engine::record::{PropertyKeyRecord, RoleNameRecord, TypeNameRecord};
    let mut tx = engine.begin_write();
    tx.put_raw(Record::TypeName(TypeNameRecord {
        id: TypeId::new(TYPE_CUSTOMER),
        name: "customer".into(),
    }));
    tx.put_raw(Record::TypeName(TypeNameRecord {
        id: TypeId::new(TYPE_REGION),
        name: "region".into(),
    }));
    tx.put_raw(Record::TypeName(TypeNameRecord {
        id: TypeId::new(TYPE_SALES),
        name: "sales".into(),
    }));
    tx.put_raw(Record::RoleName(RoleNameRecord {
        id: RoleId::new(ROLE_BUYER),
        name: "buyer".into(),
    }));
    tx.put_raw(Record::PropertyKey(PropertyKeyRecord {
        id: PropertyId::new(PROP_NAME),
        name: "name".into(),
    }));
    tx.put_raw(Record::PropertyKey(PropertyKeyRecord {
        id: PropertyId::new(PROP_REGION),
        name: "region".into(),
    }));
    tx.put_raw(Record::PropertyKey(PropertyKeyRecord {
        id: PropertyId::new(PROP_CODE),
        name: "code".into(),
    }));
    tx.commit().unwrap();
}

fn load_regions(engine: &mut Engine) -> Vec<String> {
    let mut codes = Vec::with_capacity(N_REGIONS);
    let mut tx = engine.begin_write();
    let mut in_tx = 0;
    for i in 0..N_REGIONS {
        let code = format!("REG-{i:05}");
        tx.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(TYPE_REGION),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (
                    PropertyId::new(PROP_NAME),
                    Value::String(format!("Region {i}")),
                ),
                (PropertyId::new(PROP_CODE), Value::String(code.clone())),
            ],
        });
        codes.push(code);
        in_tx += 1;
        if in_tx >= 500 {
            tx.commit().unwrap();
            tx = engine.begin_write();
            in_tx = 0;
        }
    }
    if in_tx > 0 {
        tx.commit().unwrap();
    }
    codes
}

fn load_customers(engine: &mut Engine, region_codes: &[String]) -> Vec<EntityId> {
    let mut ids = Vec::with_capacity(N_CUSTOMERS);
    let mut tx = engine.begin_write();
    let mut in_tx = 0;
    for i in 0..N_CUSTOMERS {
        let eid = EntityId::now_v7();
        let region = &region_codes[i % region_codes.len()];
        tx.put_entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(TYPE_CUSTOMER),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (
                    PropertyId::new(PROP_NAME),
                    Value::String(format!("Customer {i}")),
                ),
                (PropertyId::new(PROP_REGION), Value::String(region.clone())),
            ],
        });
        ids.push(eid);
        in_tx += 1;
        if in_tx >= 500 {
            tx.commit().unwrap();
            tx = engine.begin_write();
            in_tx = 0;
        }
    }
    if in_tx > 0 {
        tx.commit().unwrap();
    }
    ids
}

fn load_sales(engine: &mut Engine, customers: &[EntityId]) -> Vec<HyperedgeId> {
    let mut ids = Vec::with_capacity(N_SALES_ORDERS);
    let mut tx = engine.begin_write();
    let mut in_tx = 0;
    for i in 0..N_SALES_ORDERS {
        let cust_idx = if i % 2 == 0 {
            ((i / 2) % customers.len().div_ceil(20)) * 20 % customers.len()
        } else {
            (i.wrapping_mul(31).wrapping_add(7)) % customers.len()
        };
        let hid = HyperedgeId::now_v7();
        tx.put_hyperedge(HyperEdgeRecord {
            hyperedge_id: hid,
            type_id: TypeId::new(TYPE_SALES),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(ROLE_BUYER), customers[cust_idx])],
            hyperedge_roles: Vec::new(),
            properties: vec![],
        });
        ids.push(hid);
        in_tx += 1;
        if in_tx >= 500 {
            tx.commit().unwrap();
            tx = engine.begin_write();
            in_tx = 0;
        }
    }
    if in_tx > 0 {
        tx.commit().unwrap();
    }
    ids
}
