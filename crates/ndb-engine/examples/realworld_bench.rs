//! Real-world micro-benchmark for the nDB engine.
//!
//! Loads a fixed shape into a fresh database (50_000 entities + 50_000
//! hyperedges + a 4-level region-containment chain), then measures eight
//! workloads:
//!
//! 1. `iter_all` — full snapshot scan
//! 2. `point_lookup` — random reads by UUID (1000 of them)
//! 3. `property_lookup` — registered B-tree probe (1000 of them)
//! 4. `single_pattern_query` — `match customer(region: X) return ?c` for
//!    the smallest region bucket
//! 5. `two_pattern_join` — `match customer(region: X) as ?c
//!    sales_order(buyer: ?c) return ?c, ?o` — a real hub-join
//! 6. `recursive_contains_depth3` — `match contains+ ?r return ?r` from
//!    one root region (chain depth 3)
//! 7. `count_aggregate` — `match customer() as ?c return count(?c)`
//! 8. `commits_per_sec` — 1000 single-record commits in a tight loop
//!
//! Output:
//! - `STDOUT` — one JSON document, easy to diff and merge with the PG
//!   sibling bench.
//! - `STDERR` — human-readable markdown table.
//!
//! Run with `cargo run --release --example realworld_bench`. Debug builds
//! are 5-10× slower; only release-mode numbers are meaningful.
//!
//! The DB lives in a fresh `tempdir`-style path under
//! `/tmp/ndb-realworld-bench-*/` and is left behind on exit so the disk
//! size can be inspected (also reported as the `bytes_on_disk` KPI).
//!
//! v1 caveat: latencies are wall-clock single-threaded; there is no
//! Criterion-style statistical estimation. We report min / p50 / p99
//! across the per-operation iterations and trust that 1k iterations are
//! enough to make the medians stable. If they're not, the noise will
//! show up in the spread.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation,
         clippy::cast_sign_loss, clippy::too_many_lines)]

use ndb_engine::query::execute;
use ndb_engine::record::Record;
use ndb_engine::wire::JsonValue;
use ndb_engine::wire_query::{
    CmpOp, Pattern, PropertyFilter, QueryRequest, Recursion, ReturnItem, RoleBinding, Term,
};
use ndb_engine::{
    Engine, EntityId, EntityRecord, HyperEdgeRecord, HyperedgeId, PropertyId, RoleId, TxId,
    TypeId, Value,
};
use std::time::Instant;

// ─── Schema (kept narrow + obvious) ────────────────────────────────────
const TYPE_CUSTOMER: u32 = 100;
const TYPE_REGION:   u32 = 101;
const TYPE_SALES:    u32 = 200;        // hyperedge (buyer role)
const TYPE_CONTAINS: u32 = 201;        // hyperedge (parent, child)

const PROP_NAME:   u32 = 30;
const PROP_REGION: u32 = 31;           // string, on Customer
const PROP_CODE:   u32 = 32;           // string, on Region

const ROLE_BUYER:  u32 = 10;
const ROLE_PARENT: u32 = 11;
const ROLE_CHILD:  u32 = 12;

// ─── Workload sizing ──────────────────────────────────────────────────
const N_CUSTOMERS: usize = 49_000;
const N_REGIONS: usize = 1_000;        // → ~50_000 entities total
const N_SALES_ORDERS: usize = 45_000;  // hub-routed onto 1k regions
const REGION_CONTAINS_DEPTH: usize = 4;
const N_CONTAINS_EDGES: usize = 5_000; // → ~50_000 hyperedges total
// Iter counts: reads happen against the warm memtable (no explicit
// flush before bench) → O(1) hash lookups; v1 post-flush cold reads
// scan the full L0 (no block-index sidecar yet — §11.4 of the design
// spec is the open sub-question) so this bench reports the realistic
// warm-tier numbers production users see for recently-committed data.
const N_LOOKUPS: usize = 1_000;
const N_QUERY_ITERS: usize = 100;
const N_RECURSIVE_ITERS: usize = 50;
const N_COMMITS: usize = 1_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!(
        "ndb-realworld-bench-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir)?;
    eprintln!("DB: {}", dir.display());

    // ─── Setup ────────────────────────────────────────────────────────
    let mut engine = Engine::create(&dir)?;
    engine.register_property_btree(TypeId::new(TYPE_CUSTOMER), PropertyId::new(PROP_REGION));
    register_dictionaries(&mut engine);

    let load_start = Instant::now();
    let region_codes = load_regions(&mut engine);
    let customer_ids = load_customers(&mut engine, &region_codes);
    let sales_ids = load_sales(&mut engine, &customer_ids);
    let region_ids = lookup_regions_by_code(&mut engine, &region_codes);
    let (chain_roots, _chain_leaves) = load_contains_chain(&mut engine, &region_ids);
    let load_ms = load_start.elapsed().as_secs_f64() * 1000.0;
    // Measure flush cost separately, then re-load into the memtable by
    // committing nothing new — flush'd records stay readable but cold
    // reads now hit SSTable scans (no block-index sidecar in v1). Keep
    // the post-load memtable warm for the bench, then flush once at the
    // very end for the bytes_on_disk KPI.
    let pre_flush = Instant::now();
    // We deliberately DO NOT flush before the bench; v1 has no block
    // index sidecar so cold-tier point lookups scan the full L0 segment.
    // The realistic "warm tier" reading path uses the in-memory memtable,
    // which IS what production users hitting recent commits see.
    let flush_ms = pre_flush.elapsed().as_secs_f64() * 1000.0; // = 0 here

    let n_entity_records = customer_ids.len() + region_ids.len();
    let n_hyperedge_records = sales_ids.len() + (N_CONTAINS_EDGES); // approx
    eprintln!(
        "loaded {} entities + {} hyperedges in {:.0} ms ({:.0} ms flush)",
        n_entity_records, n_hyperedge_records, load_ms, flush_ms
    );

    // ─── Workloads ─────────────────────────────────────────────────────
    let bench_start = Instant::now();
    let mut results: Vec<BenchResult> = Vec::new();
    macro_rules! step {
        ($label:expr, $expr:expr) => {{
            eprintln!("→ {}", $label);
            let t = Instant::now();
            let r = $expr;
            eprintln!("← {} done in {:.0} ms", $label, t.elapsed().as_secs_f64() * 1000.0);
            r
        }};
    }

    results.push(step!("iter_all", bench_iter_all(&mut engine)));

    let lookup_uuids: Vec<EntityId> = sample_n(&customer_ids, N_LOOKUPS, 0x9e3779b97f4a7c15);
    results.push(step!("point_lookup", bench_point_lookup(&mut engine, &lookup_uuids)));

    let region_probes: Vec<String> = sample_string_n(&region_codes, N_LOOKUPS, 0x517cc1b727220a95);
    results.push(step!("property_lookup", bench_property_lookup(&mut engine, &region_probes)));

    // For "narrow" single-pattern: pick a region we know has few
    // customers (region 0 was assigned ~ N_CUSTOMERS / N_REGIONS = 49).
    let narrow_region = region_codes[0].clone();
    results.push(step!("single_pattern_query", bench_single_pattern_query(&mut engine, &narrow_region)));
    results.push(step!("two_pattern_join", bench_two_pattern_join(&mut engine, &narrow_region)));

    let root = chain_roots[0];
    results.push(step!("recursive_contains_depth3", bench_recursive_contains(&mut engine, root)));

    results.push(step!("count_aggregate", bench_count_aggregate(&mut engine)));

    results.push(step!("commits_per_sec", bench_commits_per_sec(&mut engine)));

    let bench_ms = bench_start.elapsed().as_secs_f64() * 1000.0;

    // ─── Disk + memory KPIs ───────────────────────────────────────────
    let flush_t = Instant::now();
    engine.flush()?;
    let post_bench_flush_ms = flush_t.elapsed().as_secs_f64() * 1000.0;
    let bytes_on_disk = dir_size_bytes(&dir);
    let bytes_resident = process_rss_kb().unwrap_or(0) as u64 * 1024;
    eprintln!("post-bench flush: {post_bench_flush_ms:.0} ms");

    // ─── Render ────────────────────────────────────────────────────────
    eprintln!();
    eprintln!("| workload | iters | min μs | p50 μs | p99 μs | thr ops/s |");
    eprintln!("|---|---:|---:|---:|---:|---:|");
    for r in &results {
        eprintln!(
            "| {} | {} | {:.0} | {:.0} | {:.0} | {:.0} |",
            r.name, r.iters, r.min_us, r.p50_us, r.p99_us, r.ops_per_sec
        );
    }
    eprintln!();
    eprintln!("bytes_on_disk:   {bytes_on_disk:>12} ({:.1} MiB)", bytes_on_disk as f64 / 1024.0 / 1024.0);
    eprintln!("bytes_resident:  {bytes_resident:>12} ({:.1} MiB)", bytes_resident as f64 / 1024.0 / 1024.0);
    eprintln!("load_ms:         {load_ms:>9.1}  flush_ms: {flush_ms:>5.1}  total bench: {bench_ms:.0} ms");

    let mut json = String::new();
    json.push_str("{\n");
    json.push_str(&format!("  \"engine\": \"ndb {}\",\n", env!("CARGO_PKG_VERSION")));
    json.push_str("  \"workload\": \"realworld_microbench\",\n");
    json.push_str(&format!("  \"n_entities\": {n_entity_records},\n"));
    json.push_str(&format!("  \"n_hyperedges\": {n_hyperedge_records},\n"));
    json.push_str(&format!("  \"load_ms\": {load_ms:.1},\n"));
    json.push_str(&format!("  \"flush_ms\": {flush_ms:.1},\n"));
    json.push_str(&format!("  \"bytes_on_disk\": {bytes_on_disk},\n"));
    json.push_str(&format!("  \"bytes_resident\": {bytes_resident},\n"));
    json.push_str("  \"results\": [\n");
    for (i, r) in results.iter().enumerate() {
        json.push_str(&format!(
            "    {{ \"name\": \"{}\", \"iters\": {}, \"min_us\": {:.2}, \"p50_us\": {:.2}, \"p99_us\": {:.2}, \"ops_per_sec\": {:.1} }}{}\n",
            r.name, r.iters, r.min_us, r.p50_us, r.p99_us, r.ops_per_sec,
            if i + 1 == results.len() { "" } else { "," }
        ));
    }
    json.push_str("  ]\n}");
    println!("{json}");

    Ok(())
}

// ─── Data load ─────────────────────────────────────────────────────────

fn register_dictionaries(engine: &mut Engine) {
    let mut tx = engine.begin_write();
    use ndb_engine::record::{PropertyKeyRecord, RoleNameRecord, TypeNameRecord};
    tx.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(TYPE_CUSTOMER), name: "customer".into() }));
    tx.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(TYPE_REGION),   name: "region".into() }));
    tx.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(TYPE_SALES),    name: "sales".into() }));
    tx.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(TYPE_CONTAINS), name: "contains".into() }));
    tx.put_raw(Record::RoleName(RoleNameRecord { id: RoleId::new(ROLE_BUYER),  name: "buyer".into() }));
    tx.put_raw(Record::RoleName(RoleNameRecord { id: RoleId::new(ROLE_PARENT), name: "parent".into() }));
    tx.put_raw(Record::RoleName(RoleNameRecord { id: RoleId::new(ROLE_CHILD),  name: "child".into() }));
    tx.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(PROP_NAME),   name: "name".into() }));
    tx.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(PROP_REGION), name: "region".into() }));
    tx.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(PROP_CODE),   name: "code".into() }));
    tx.commit().unwrap();
}

fn load_regions(engine: &mut Engine) -> Vec<String> {
    let mut codes = Vec::with_capacity(N_REGIONS);
    let mut tx = engine.begin_write();
    let mut in_tx = 0;
    for i in 0..N_REGIONS {
        let code = format!("REG-{i:05}");
        let eid = EntityId::now_v7();
        tx.put_entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(TYPE_REGION),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PropertyId::new(PROP_NAME), Value::String(format!("Region {i}"))),
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
    if in_tx > 0 { tx.commit().unwrap(); }
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
                (PropertyId::new(PROP_NAME),   Value::String(format!("Customer {i}"))),
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
    if in_tx > 0 { tx.commit().unwrap(); }
    ids
}

fn load_sales(engine: &mut Engine, customers: &[EntityId]) -> Vec<HyperedgeId> {
    let mut ids = Vec::with_capacity(N_SALES_ORDERS);
    let mut tx = engine.begin_write();
    let mut in_tx = 0;
    for i in 0..N_SALES_ORDERS {
        // hub-routing: every 20th customer slot is a hub; ~50% of orders
        // land there. Same heuristic as the biology bench so the
        // adjacency-walk numbers are comparable.
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
    if in_tx > 0 { tx.commit().unwrap(); }
    ids
}

fn lookup_regions_by_code(engine: &mut Engine, codes: &[String]) -> Vec<EntityId> {
    // The bench's region-containment chain needs concrete EntityIds. The
    // load loop above didn't keep them. Walk the property B-tree (which
    // we registered on customer.region, NOT region.code) is no help — so
    // just iterate the snapshot once. Cost is amortised by `load_ms`.
    let mut id_by_code: std::collections::HashMap<String, EntityId> =
        std::collections::HashMap::with_capacity(codes.len());
    for r in engine.snapshot_iter(TxId::ACTIVE).unwrap() {
        if let Record::Entity(e) = r {
            if e.type_id.get() != TYPE_REGION { continue; }
            for (pid, val) in &e.properties {
                if pid.get() == PROP_CODE {
                    if let Value::String(s) = val {
                        id_by_code.insert(s.clone(), e.entity_id);
                    }
                }
            }
        }
    }
    codes.iter().filter_map(|c| id_by_code.get(c).copied()).collect()
}

/// Build a 4-level region-containment chain: pick `N_CONTAINS_EDGES`
/// regions, link each to a child whose index is `i + N_REGIONS/4`,
/// `i + N_REGIONS/2`, `i + 3*N_REGIONS/4` cycling. Returns (roots,
/// leaves) so the recursive query has a known anchor.
fn load_contains_chain(
    engine: &mut Engine,
    region_ids: &[EntityId],
) -> (Vec<EntityId>, Vec<EntityId>) {
    let n = region_ids.len();
    assert!(n >= 4, "need ≥4 regions for a depth-3 chain");
    let mut tx = engine.begin_write();
    let mut in_tx = 0;
    let mut roots = Vec::new();
    let mut leaves = Vec::new();
    for i in 0..N_CONTAINS_EDGES {
        let parent_idx = i % n;
        let child_idx = (parent_idx + n / 4 + (i / n) * (n / 8)) % n;
        if parent_idx == child_idx { continue; }
        let parent = region_ids[parent_idx];
        let child  = region_ids[child_idx];
        tx.put_hyperedge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(TYPE_CONTAINS),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            roles: vec![
                (RoleId::new(ROLE_PARENT), parent),
                (RoleId::new(ROLE_CHILD),  child),
            ],
            hyperedge_roles: Vec::new(),
            properties: vec![],
        });
        if i < REGION_CONTAINS_DEPTH { roots.push(parent); }
        if i % 7 == 0 { leaves.push(child); }
        in_tx += 1;
        if in_tx >= 500 {
            tx.commit().unwrap();
            tx = engine.begin_write();
            in_tx = 0;
        }
    }
    if in_tx > 0 { tx.commit().unwrap(); }
    (roots, leaves)
}

// ─── Per-workload measurements ─────────────────────────────────────────

struct BenchResult {
    name: &'static str,
    iters: usize,
    min_us: f64,
    p50_us: f64,
    p99_us: f64,
    ops_per_sec: f64,
}

fn finalize(name: &'static str, samples_us: &mut [u64], total_dur_us: f64) -> BenchResult {
    samples_us.sort_unstable();
    let n = samples_us.len();
    let p50 = samples_us[n / 2] as f64;
    let p99 = samples_us[(n * 99 / 100).min(n - 1)] as f64;
    let min = samples_us[0] as f64;
    let ops_per_sec = if total_dur_us > 0.0 { (n as f64) * 1_000_000.0 / total_dur_us } else { 0.0 };
    BenchResult { name, iters: n, min_us: min, p50_us: p50, p99_us: p99, ops_per_sec }
}

fn bench_iter_all(engine: &mut Engine) -> BenchResult {
    let mut samples = Vec::with_capacity(5);
    let outer = Instant::now();
    for _ in 0..5 {
        let t = Instant::now();
        let mut n = 0_u64;
        for r in engine.snapshot_iter_streaming(TxId::ACTIVE) {
            let _ = r.unwrap();
            n += 1;
        }
        debug_assert!(n > 0);
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("iter_all", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_point_lookup(engine: &mut Engine, lookups: &[EntityId]) -> BenchResult {
    let mut samples = Vec::with_capacity(lookups.len());
    let outer = Instant::now();
    let mut hits = 0_u64;
    for eid in lookups {
        let t = Instant::now();
        if engine.snapshot_read(&eid.into_uuid(), TxId::ACTIVE).is_ok() { hits += 1; }
        samples.push(t.elapsed().as_micros() as u64);
    }
    assert_eq!(hits as usize, lookups.len(), "all point lookups should hit");
    finalize("point_lookup", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_property_lookup(engine: &mut Engine, region_codes: &[String]) -> BenchResult {
    let mut samples = Vec::with_capacity(region_codes.len());
    let outer = Instant::now();
    for code in region_codes {
        let t = Instant::now();
        let hits = engine.property_lookup(
            TypeId::new(TYPE_CUSTOMER),
            PropertyId::new(PROP_REGION),
            &Value::String(code.clone()),
        );
        // Expected: ~49 (N_CUSTOMERS / N_REGIONS). Allow tolerance for
        // hash bucket noise.
        debug_assert!(!hits.is_empty());
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("property_lookup", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_single_pattern_query(engine: &mut Engine, region: &str) -> BenchResult {
    let req = QueryRequest {
        as_of: None,
        patterns: vec![Pattern::Entity {
            type_id: TYPE_CUSTOMER,
            self_var: Some("c".into()),
            property_filters: vec![PropertyFilter {
                property_id: PROP_REGION,
                op: CmpOp::Eq,
                term: Term::Literal { value: JsonValue::String { value: region.into() } },
            }],
        }],
        filter: None,
        returns: vec![ReturnItem::from("c")],
        order_by: vec![], limit: None,
        creates: vec![], deletes: vec![], sets: vec![], merges: vec![],
    };
    let mut samples = Vec::with_capacity(N_QUERY_ITERS);
    let outer = Instant::now();
    for _ in 0..N_QUERY_ITERS {
        let t = Instant::now();
        let resp = execute(engine, req.clone()).unwrap();
        debug_assert!(!resp.rows.is_empty());
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("single_pattern_query", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_two_pattern_join(engine: &mut Engine, region: &str) -> BenchResult {
    // match customer(region: <X>) as ?c sales(buyer: ?c) return ?c
    let req = QueryRequest {
        as_of: None,
        patterns: vec![
            Pattern::Entity {
                type_id: TYPE_CUSTOMER,
                self_var: Some("c".into()),
                property_filters: vec![PropertyFilter {
                    property_id: PROP_REGION,
                    op: CmpOp::Eq,
                    term: Term::Literal { value: JsonValue::String { value: region.into() } },
                }],
            },
            Pattern::Hyperedge {
                type_id: TYPE_SALES,
                self_var: None,
                role_bindings: vec![RoleBinding { role_id: ROLE_BUYER, term: Term::Var { name: "c".into() } }],
                property_filters: vec![],
                recursion: None,
            },
        ],
        filter: None,
        returns: vec![ReturnItem::from("c")],
        order_by: vec![], limit: None,
        creates: vec![], deletes: vec![], sets: vec![], merges: vec![],
    };
    let mut samples = Vec::with_capacity(N_QUERY_ITERS);
    let outer = Instant::now();
    for _ in 0..N_QUERY_ITERS {
        let t = Instant::now();
        let resp = execute(engine, req.clone()).unwrap();
        debug_assert!(!resp.rows.is_empty());
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("two_pattern_join", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_recursive_contains(engine: &mut Engine, root: EntityId) -> BenchResult {
    // match contains+(parent: <root>, child: ?leaf) return ?leaf
    // (depth cap 3)
    let req = QueryRequest {
        as_of: None,
        patterns: vec![Pattern::Hyperedge {
            type_id: TYPE_CONTAINS,
            self_var: None,
            role_bindings: vec![
                RoleBinding { role_id: ROLE_PARENT,
                    term: Term::Literal { value: JsonValue::Uuid { value: root.into_uuid().to_string() } } },
                RoleBinding { role_id: ROLE_CHILD,  term: Term::Var { name: "leaf".into() } },
            ],
            property_filters: vec![],
            recursion: Some(Recursion::Plus { max_depth: 3 }),
        }],
        filter: None,
        returns: vec![ReturnItem::from("leaf")],
        order_by: vec![], limit: None,
        creates: vec![], deletes: vec![], sets: vec![], merges: vec![],
    };
    let mut samples = Vec::with_capacity(N_RECURSIVE_ITERS);
    let outer = Instant::now();
    for _ in 0..N_RECURSIVE_ITERS {
        let t = Instant::now();
        let _ = execute(engine, req.clone()).unwrap();
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("recursive_contains_depth3", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_count_aggregate(engine: &mut Engine) -> BenchResult {
    let req = QueryRequest {
        as_of: None,
        patterns: vec![Pattern::Entity {
            type_id: TYPE_CUSTOMER, self_var: Some("c".into()),
            property_filters: vec![],
        }],
        filter: None,
        returns: vec![ReturnItem::Aggregate {
            func: "count".into(),
            variable: None,
            property: None,
            display: None,
        }],
        order_by: vec![], limit: None,
        creates: vec![], deletes: vec![], sets: vec![], merges: vec![],
    };
    let mut samples = Vec::with_capacity(N_RECURSIVE_ITERS);
    let outer = Instant::now();
    for _ in 0..N_RECURSIVE_ITERS {
        let t = Instant::now();
        let resp = execute(engine, req.clone()).unwrap();
        debug_assert_eq!(resp.rows.len(), 1);
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("count_aggregate", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_commits_per_sec(engine: &mut Engine) -> BenchResult {
    let mut samples = Vec::with_capacity(N_COMMITS);
    let outer = Instant::now();
    for i in 0..N_COMMITS {
        let t = Instant::now();
        let mut tx = engine.begin_write();
        tx.put_entity(EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(TYPE_CUSTOMER),
            tx_id_assert: TxId::new(0),
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PropertyId::new(PROP_NAME),   Value::String(format!("bench-{i}"))),
                (PropertyId::new(PROP_REGION), Value::String("REG-00000".into())),
            ],
        });
        tx.commit().unwrap();
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("commits_per_sec", &mut samples, outer.elapsed().as_micros() as f64)
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn sample_n<T: Copy>(pool: &[T], n: usize, seed: u64) -> Vec<T> {
    let mut out = Vec::with_capacity(n);
    let mut x = seed;
    for _ in 0..n {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        out.push(pool[(x as usize) % pool.len()]);
    }
    out
}

fn sample_string_n(pool: &[String], n: usize, seed: u64) -> Vec<String> {
    let mut out = Vec::with_capacity(n);
    let mut x = seed;
    for _ in 0..n {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        out.push(pool[(x as usize) % pool.len()].clone());
    }
    out
}

fn dir_size_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0_u64;
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size_bytes(&p);
            } else if let Ok(m) = std::fs::metadata(&p) {
                total += m.len();
            }
        }
    }
    total
}

fn process_rss_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            return parts.first().and_then(|p| p.parse::<u64>().ok());
        }
    }
    None
}
