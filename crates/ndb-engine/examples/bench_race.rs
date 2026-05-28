//! Live bench-race HTTP server for the knowledge site.
//!
//! Loads the same realworld dataset as `examples/realworld_bench.rs`
//! (50_000 entities + 50_000 hyperedges) into a fresh nDB at startup,
//! then exposes a tiny HTTP/1.1 surface so the `bench.html` page can
//! POST `/run/<workload>` and watch live timings race against the
//! Postgres sibling at `tools/bench/race_pg_server.py`.
//!
//! Surface:
//!   - `GET  /health`     — `{"status":"ok","loaded":true}` once data is in
//!   - `GET  /workloads`  — list of `{name, label, iters, blurb}` cards
//!   - `POST /run/<name>` — runs the workload N times (`iters` capped to
//!                          fit in ~500ms wall-clock), returns
//!                          `{name, iters, min_us, p50_us, p99_us,
//!                            ops_per_sec, total_ms}`
//!   - CORS open (single-origin behind the knowledge-site proxy in
//!     practice; the open headers cost nothing on a read-only surface).
//!
//! Read-only: no `/run/commits_per_sec` (write workload — would mutate
//! the shared state visible to every visitor). All other workloads are
//! pure reads.
//!
//! Rate limit: 1 race per IP per workload per `RATE_LIMIT_SECS`.
//!
//! Run with:
//!     cargo run --release --example bench_race -- --bind 127.0.0.1:8771
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
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ─── Schema (kept narrow + obvious) ────────────────────────────────────
const TYPE_CUSTOMER: u32 = 100;
const TYPE_REGION:   u32 = 101;
const TYPE_SALES:    u32 = 200;
const TYPE_CONTAINS: u32 = 201;

const PROP_NAME:   u32 = 30;
const PROP_REGION: u32 = 31;
const PROP_CODE:   u32 = 32;

const ROLE_BUYER:  u32 = 10;
const ROLE_PARENT: u32 = 11;
const ROLE_CHILD:  u32 = 12;

// ─── Workload sizing (smaller than the realworld bench — live = must
// stay under ~500 ms wall-clock per click). ──────────────────────────
const N_CUSTOMERS: usize = 49_000;
const N_REGIONS: usize = 1_000;
const N_SALES_ORDERS: usize = 45_000;
const N_CONTAINS_EDGES: usize = 5_000;

const N_ITER_LOOKUPS: usize = 500;
const N_ITER_QUERY_SMALL: usize = 50;
const N_ITER_QUERY_LARGE: usize = 10;  // two_pattern_join / count_aggregate
const N_ITER_RECURSIVE: usize = 25;
const N_ITER_ITERATE: usize = 3;

const RATE_LIMIT_SECS: u64 = 3;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind = std::env::args()
        .skip(1)
        .scan(false, |seen, a| {
            if *seen { *seen = false; Some(Some(a)) }
            else if a == "--bind" { *seen = true; Some(None) }
            else { Some(None) }
        })
        .flatten()
        .next()
        .unwrap_or_else(|| "127.0.0.1:8771".into());

    let dir = std::env::temp_dir().join(format!(
        "ndb-bench-race-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir)?;
    eprintln!("DB: {}", dir.display());

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
    let n_entities = customer_ids.len() + region_ids.len();
    let n_hyperedges = sales_ids.len() + N_CONTAINS_EDGES;
    eprintln!(
        "loaded {} entities + {} hyperedges in {:.0} ms",
        n_entities, n_hyperedges, load_ms
    );

    let lookup_uuids = sample_n(&customer_ids, N_ITER_LOOKUPS, 0x9e3779b97f4a7c15);
    let region_probes = sample_string_n(&region_codes, N_ITER_LOOKUPS, 0x517cc1b727220a95);

    let state = State {
        engine: Mutex::new(engine),
        narrow_region: region_codes[0].clone(),
        lookup_uuids,
        region_probes,
        chain_root: chain_roots[0],
        n_entities,
        n_hyperedges,
        load_ms,
        rate_limiter: Mutex::new(HashMap::new()),
    };
    let state = std::sync::Arc::new(state);

    let listener = TcpListener::bind(&bind)?;
    eprintln!("bench-race nDB serving on http://{bind}");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let state = std::sync::Arc::clone(&state);
                std::thread::spawn(move || {
                    if let Err(e) = handle(state, s) {
                        eprintln!("connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

// ─── State + workload registry ────────────────────────────────────────

struct State {
    engine: Mutex<Engine>,
    narrow_region: String,
    lookup_uuids: Vec<EntityId>,
    region_probes: Vec<String>,
    chain_root: EntityId,
    n_entities: usize,
    n_hyperedges: usize,
    load_ms: f64,
    rate_limiter: Mutex<HashMap<(String, String), Instant>>,
}

struct Workload {
    name: &'static str,
    label: &'static str,
    blurb: &'static str,
    iters: usize,
}

const WORKLOADS: &[Workload] = &[
    Workload { name: "iter_all", label: "Full snapshot scan",
        blurb: "Streaming walk of every entity + hyperedge.", iters: N_ITER_ITERATE },
    Workload { name: "point_lookup", label: "Random point lookup",
        blurb: "Fetch one record by UUID, 500 times.", iters: N_ITER_LOOKUPS },
    Workload { name: "property_lookup", label: "Indexed property lookup",
        blurb: "Look up customers by region code via the B-tree, 500 times.", iters: N_ITER_LOOKUPS },
    Workload { name: "single_pattern_query", label: "Single-pattern query",
        blurb: "match customer(region: \"REG-00000\") as ?c return ?c", iters: N_ITER_QUERY_SMALL },
    Workload { name: "two_pattern_join", label: "Two-pattern join",
        blurb: "match customer(region: X) as ?c sales(buyer: ?c) return ?c", iters: N_ITER_QUERY_LARGE },
    Workload { name: "recursive_contains_depth3", label: "Recursive walk, depth 3",
        blurb: "match contains+(parent: <root>, child: ?leaf) {1,3}", iters: N_ITER_RECURSIVE },
    Workload { name: "count_aggregate", label: "count() over a type",
        blurb: "Aggregate over 49k customer entities.", iters: N_ITER_QUERY_LARGE },
];

// ─── HTTP loop ─────────────────────────────────────────────────────────

fn handle(state: Arc<State>, mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let peer_ip = stream.peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "?".into());

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut method_path = request_line.trim_end().split_whitespace();
    let method = method_path.next().unwrap_or("").to_string();
    let path = method_path.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 { break; }
        let trimmed = header.trim_end();
        if trimmed.is_empty() { break; }
        if let Some(v) = trimmed.strip_prefix("Content-Length:").or_else(|| trimmed.strip_prefix("content-length:")) {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = Vec::new();
    if content_length > 0 {
        body.resize(content_length, 0u8);
        reader.read_exact(&mut body)?;
    }

    match (method.as_str(), path.as_str()) {
        ("GET", "/health") => {
            send_json(&mut stream, 200, &format!(
                "{{\"status\":\"ok\",\"loaded\":true,\"engine\":\"ndb {}\",\
                 \"n_entities\":{},\"n_hyperedges\":{},\"load_ms\":{:.0}}}",
                env!("CARGO_PKG_VERSION"),
                state.n_entities, state.n_hyperedges, state.load_ms,
            ))
        }
        ("GET", "/workloads") => {
            let mut out = String::from("[");
            for (i, w) in WORKLOADS.iter().enumerate() {
                if i > 0 { out.push(','); }
                out.push_str(&format!(
                    "{{\"name\":\"{}\",\"label\":\"{}\",\"blurb\":\"{}\",\"iters\":{}}}",
                    w.name,
                    escape_json(w.label),
                    escape_json(w.blurb),
                    w.iters,
                ));
            }
            out.push(']');
            send_json(&mut stream, 200, &out)
        }
        ("POST", path) if path.starts_with("/run/") => {
            let name = &path["/run/".len()..];
            let Some(workload) = WORKLOADS.iter().find(|w| w.name == name) else {
                return send_json(&mut stream, 404,
                    "{\"error\":\"unknown_workload\"}");
            };
            // Rate limit
            {
                let mut rl = state.rate_limiter.lock().unwrap();
                let key = (peer_ip.clone(), name.to_string());
                let now = Instant::now();
                if let Some(prev) = rl.get(&key) {
                    let dt = now.duration_since(*prev);
                    if dt < Duration::from_secs(RATE_LIMIT_SECS) {
                        let wait = RATE_LIMIT_SECS - dt.as_secs();
                        return send_json(&mut stream, 429,
                            &format!("{{\"error\":\"rate_limit\",\"retry_after_s\":{wait}}}"));
                    }
                }
                rl.insert(key, now);
                // GC every 256 entries
                if rl.len() > 256 {
                    rl.retain(|_, t| now.duration_since(*t) < Duration::from_secs(60));
                }
            }
            let result = run_workload(&state, workload);
            send_json(&mut stream, 200, &result)
        }
        ("POST", "/stress") => {
            let req: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => return send_json(&mut stream, 400,
                    &format!("{{\"error\":\"bad_json\",\"detail\":\"{}\"}}",
                             escape_json(&e.to_string()))),
            };
            let workload_name = req.get("workload").and_then(|v| v.as_str()).unwrap_or("");
            let Some(workload) = WORKLOADS.iter().find(|w| w.name == workload_name) else {
                return send_json(&mut stream, 404,
                    "{\"error\":\"unknown_workload\"}");
            };
            let concurrency = req.get("concurrency").and_then(|v| v.as_u64()).unwrap_or(4) as usize;
            let duration_ms = req.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(5000);
            let concurrency = concurrency.clamp(1, 128);
            let duration_ms = duration_ms.clamp(500, 30_000);
            // Stress rate limit: per-IP, regardless of workload — these
            // ops are heavy.
            {
                let mut rl = state.rate_limiter.lock().unwrap();
                let key = (peer_ip.clone(), "/stress".to_string());
                let now = Instant::now();
                if let Some(prev) = rl.get(&key) {
                    let dt = now.duration_since(*prev);
                    if dt < Duration::from_secs(RATE_LIMIT_SECS) {
                        let wait = RATE_LIMIT_SECS - dt.as_secs();
                        return send_json(&mut stream, 429,
                            &format!("{{\"error\":\"rate_limit\",\"retry_after_s\":{wait}}}"));
                    }
                }
                rl.insert(key, now);
            }
            let result = run_stress(&state, workload, concurrency, duration_ms);
            send_json(&mut stream, 200, &result)
        }
        _ => send_json(&mut stream, 404, "{\"error\":\"not_found\"}"),
    }
}

fn send_json(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let status_text = match status {
        200 => "OK", 404 => "Not Found", 429 => "Too Many Requests", 500 => "Internal Server Error",
        _ => "OK",
    };
    let resp = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: application/json\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         Content-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ─── Workload execution ───────────────────────────────────────────────

fn run_workload(state: &State, workload: &Workload) -> String {
    let mut engine = state.engine.lock().unwrap();
    let r = match workload.name {
        "iter_all"                  => bench_iter_all(&mut engine, workload.iters),
        "point_lookup"              => bench_point_lookup(&mut engine, &state.lookup_uuids),
        "property_lookup"           => bench_property_lookup(&mut engine, &state.region_probes),
        "single_pattern_query"      => bench_single_pattern_query(&mut engine, &state.narrow_region, workload.iters),
        "two_pattern_join"          => bench_two_pattern_join(&mut engine, &state.narrow_region, workload.iters),
        "recursive_contains_depth3" => bench_recursive_contains(&mut engine, state.chain_root, workload.iters),
        "count_aggregate"           => bench_count_aggregate(&mut engine, workload.iters),
        _ => unreachable!(),
    };
    format!(
        "{{\"name\":\"{}\",\"iters\":{},\"min_us\":{:.0},\"p50_us\":{:.0},\
         \"p99_us\":{:.0},\"ops_per_sec\":{:.1},\"total_ms\":{:.1}}}",
        r.name, r.iters, r.min_us, r.p50_us, r.p99_us, r.ops_per_sec, r.total_ms,
    )
}

// ─── Concurrent stress runner ─────────────────────────────────────────
//
// Spawns `concurrency` worker threads. Each thread loops until the
// deadline expires: acquire `state.engine` mutex, run one op, record
// latency in its local Vec. Then we merge all threads' Vecs, compute
// percentiles + a log10 histogram. The engine mutex serialises ops
// (v1 single-writer), so above conc=1 the lanes just queue — that's
// the realistic v1 ceiling and we report it honestly.

fn run_stress(state: &Arc<State>, workload: &Workload, concurrency: usize, duration_ms: u64) -> String {
    let deadline = Instant::now() + Duration::from_millis(duration_ms);
    let started = Instant::now();
    let workload_name: &'static str = workload.name;

    let mut handles = Vec::with_capacity(concurrency);
    for tid in 0..concurrency {
        let st = Arc::clone(state);
        handles.push(std::thread::spawn(move || -> (Vec<u64>, u64) {
            let mut latencies: Vec<u64> = Vec::with_capacity(1024);
            let mut errors: u64 = 0;
            // Per-thread index rotation seed so workers don't all hit
            // the same lookup uuid.
            let mut idx_seed = (tid as u64).wrapping_mul(0x9e3779b97f4a7c15);
            while Instant::now() < deadline {
                idx_seed ^= idx_seed << 13;
                idx_seed ^= idx_seed >> 7;
                idx_seed ^= idx_seed << 17;
                let idx = idx_seed as usize;
                let t = Instant::now();
                let ok = {
                    let mut engine = st.engine.lock().unwrap();
                    do_one_op(&mut engine, &st, workload_name, idx)
                };
                let us = t.elapsed().as_micros() as u64;
                if ok { latencies.push(us); } else { errors += 1; }
            }
            (latencies, errors)
        }));
    }
    let mut all = Vec::with_capacity(concurrency * 1024);
    let mut errors = 0_u64;
    for h in handles {
        let (lat, err) = h.join().unwrap_or((Vec::new(), 0));
        all.extend(lat);
        errors += err;
    }
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;

    let total_ops = all.len() as u64;
    all.sort_unstable();
    let pct = |q: f64| -> u64 {
        if all.is_empty() { 0 }
        else { all[((all.len() as f64) * q).min(all.len() as f64 - 1.0) as usize] }
    };
    let p50 = pct(0.50);
    let p95 = pct(0.95);
    let p99 = pct(0.99);
    let p999 = pct(0.999);
    let max = all.last().copied().unwrap_or(0);
    let rps = if wall_ms > 0.0 { (total_ops as f64) * 1000.0 / wall_ms } else { 0.0 };

    // Log10 histogram, 6 decades (1 μs … 1 s) × 10 bins/decade = 60.
    const N_BUCKETS: usize = 60;
    let mut hist = [0u64; N_BUCKETS];
    for us in &all {
        let v = (*us).max(1);
        let b = ((v as f64).log10() * 10.0) as i64;
        let b = b.clamp(0, N_BUCKETS as i64 - 1) as usize;
        hist[b] += 1;
    }

    let mut hist_json = String::from("[");
    for (i, &count) in hist.iter().enumerate() {
        if count == 0 { continue; }
        if hist_json.len() > 1 { hist_json.push(','); }
        let edge = 10f64.powf(i as f64 / 10.0);
        hist_json.push_str(&format!("[{:.0},{}]", edge, count));
    }
    hist_json.push(']');

    format!(
        "{{\"workload\":\"{}\",\"concurrency\":{},\"duration_ms\":{},\
         \"wall_ms\":{:.1},\"total_ops\":{},\"errors\":{},\"rps\":{:.1},\
         \"p50_us\":{},\"p95_us\":{},\"p99_us\":{},\"p999_us\":{},\
         \"max_us\":{},\"histogram_log10\":{}}}",
        workload_name, concurrency, duration_ms, wall_ms,
        total_ops, errors, rps, p50, p95, p99, p999, max, hist_json,
    )
}

/// Run a single op of the named workload. Picks deterministic samples
/// using `idx` (per-thread rotating seed) for the workloads that need a
/// pool element. Returns true on success.
fn do_one_op(engine: &mut Engine, state: &State, workload: &str, idx: usize) -> bool {
    match workload {
        "iter_all" => {
            let mut n = 0_u64;
            for r in engine.snapshot_iter_streaming(TxId::ACTIVE) {
                if r.is_err() { return false; }
                n += 1;
            }
            n > 0
        }
        "point_lookup" => {
            let eid = state.lookup_uuids[idx % state.lookup_uuids.len()];
            engine.snapshot_read(&eid.into_uuid(), TxId::ACTIVE).is_ok()
        }
        "property_lookup" => {
            let code = &state.region_probes[idx % state.region_probes.len()];
            let _ = engine.property_lookup(
                TypeId::new(TYPE_CUSTOMER),
                PropertyId::new(PROP_REGION),
                &Value::String(code.clone()),
            );
            true
        }
        "single_pattern_query" => {
            let req = single_pattern_request(&state.narrow_region);
            execute(engine, req).is_ok()
        }
        "two_pattern_join" => {
            let req = two_pattern_request(&state.narrow_region);
            execute(engine, req).is_ok()
        }
        "recursive_contains_depth3" => {
            let req = recursive_request(state.chain_root);
            execute(engine, req).is_ok()
        }
        "count_aggregate" => {
            let req = count_request();
            execute(engine, req).is_ok()
        }
        _ => false,
    }
}

struct BenchResult {
    name: &'static str,
    iters: usize,
    min_us: f64,
    p50_us: f64,
    p99_us: f64,
    ops_per_sec: f64,
    total_ms: f64,
}

fn finalize(name: &'static str, samples_us: &mut [u64], total_dur_us: f64) -> BenchResult {
    samples_us.sort_unstable();
    let n = samples_us.len();
    let p50 = samples_us[n / 2] as f64;
    let p99 = samples_us[(n * 99 / 100).min(n - 1)] as f64;
    let min = samples_us[0] as f64;
    let ops_per_sec = if total_dur_us > 0.0 { (n as f64) * 1_000_000.0 / total_dur_us } else { 0.0 };
    BenchResult { name, iters: n, min_us: min, p50_us: p50, p99_us: p99, ops_per_sec, total_ms: total_dur_us / 1000.0 }
}

fn bench_iter_all(engine: &mut Engine, iters: usize) -> BenchResult {
    let mut samples = Vec::with_capacity(iters);
    let outer = Instant::now();
    for _ in 0..iters {
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
    for eid in lookups {
        let t = Instant::now();
        let _ = engine.snapshot_read(&eid.into_uuid(), TxId::ACTIVE);
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("point_lookup", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_property_lookup(engine: &mut Engine, region_codes: &[String]) -> BenchResult {
    let mut samples = Vec::with_capacity(region_codes.len());
    let outer = Instant::now();
    for code in region_codes {
        let t = Instant::now();
        let _ = engine.property_lookup(
            TypeId::new(TYPE_CUSTOMER),
            PropertyId::new(PROP_REGION),
            &Value::String(code.clone()),
        );
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("property_lookup", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_single_pattern_query(engine: &mut Engine, region: &str, iters: usize) -> BenchResult {
    let req = single_pattern_request(region);
    let mut samples = Vec::with_capacity(iters);
    let outer = Instant::now();
    for _ in 0..iters {
        let t = Instant::now();
        let _ = execute(engine, req.clone()).unwrap();
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("single_pattern_query", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_two_pattern_join(engine: &mut Engine, region: &str, iters: usize) -> BenchResult {
    let req = two_pattern_request(region);
    let mut samples = Vec::with_capacity(iters);
    let outer = Instant::now();
    for _ in 0..iters {
        let t = Instant::now();
        let _ = execute(engine, req.clone()).unwrap();
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("two_pattern_join", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_recursive_contains(engine: &mut Engine, root: EntityId, iters: usize) -> BenchResult {
    let req = recursive_request(root);
    let mut samples = Vec::with_capacity(iters);
    let outer = Instant::now();
    for _ in 0..iters {
        let t = Instant::now();
        let _ = execute(engine, req.clone()).unwrap();
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("recursive_contains_depth3", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_count_aggregate(engine: &mut Engine, iters: usize) -> BenchResult {
    let req = count_request();
    let mut samples = Vec::with_capacity(iters);
    let outer = Instant::now();
    for _ in 0..iters {
        let t = Instant::now();
        let _ = execute(engine, req.clone()).unwrap();
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("count_aggregate", &mut samples, outer.elapsed().as_micros() as f64)
}

// ─── Pre-built QueryRequests ──────────────────────────────────────────

fn single_pattern_request(region: &str) -> QueryRequest {
    QueryRequest {
        as_of: None,
        patterns: vec![Pattern::Entity {
            type_id: TYPE_CUSTOMER, self_var: Some("c".into()),
            property_filters: vec![PropertyFilter {
                property_id: PROP_REGION, op: CmpOp::Eq,
                term: Term::Literal { value: JsonValue::String { value: region.into() } },
            }],
        }],
        filter: None, returns: vec![ReturnItem::from("c")],
        order_by: vec![], limit: None,
        creates: vec![], deletes: vec![], sets: vec![], merges: vec![],
    }
}

fn two_pattern_request(region: &str) -> QueryRequest {
    QueryRequest {
        as_of: None,
        patterns: vec![
            Pattern::Entity {
                type_id: TYPE_CUSTOMER, self_var: Some("c".into()),
                property_filters: vec![PropertyFilter {
                    property_id: PROP_REGION, op: CmpOp::Eq,
                    term: Term::Literal { value: JsonValue::String { value: region.into() } },
                }],
            },
            Pattern::Hyperedge {
                type_id: TYPE_SALES, self_var: None,
                role_bindings: vec![RoleBinding { role_id: ROLE_BUYER, term: Term::Var { name: "c".into() } }],
                property_filters: vec![], recursion: None,
            },
        ],
        filter: None, returns: vec![ReturnItem::from("c")],
        order_by: vec![], limit: None,
        creates: vec![], deletes: vec![], sets: vec![], merges: vec![],
    }
}

fn recursive_request(root: EntityId) -> QueryRequest {
    QueryRequest {
        as_of: None,
        patterns: vec![Pattern::Hyperedge {
            type_id: TYPE_CONTAINS, self_var: None,
            role_bindings: vec![
                RoleBinding { role_id: ROLE_PARENT,
                    term: Term::Literal { value: JsonValue::Uuid { value: root.into_uuid().to_string() } } },
                RoleBinding { role_id: ROLE_CHILD,  term: Term::Var { name: "leaf".into() } },
            ],
            property_filters: vec![],
            recursion: Some(Recursion::Plus { max_depth: 3 }),
        }],
        filter: None, returns: vec![ReturnItem::from("leaf")],
        order_by: vec![], limit: None,
        creates: vec![], deletes: vec![], sets: vec![], merges: vec![],
    }
}

fn count_request() -> QueryRequest {
    QueryRequest {
        as_of: None,
        patterns: vec![Pattern::Entity {
            type_id: TYPE_CUSTOMER, self_var: Some("c".into()),
            property_filters: vec![],
        }],
        filter: None,
        returns: vec![ReturnItem::Aggregate {
            func: "count".into(), variable: None, property: None, display: None,
        }],
        order_by: vec![], limit: None,
        creates: vec![], deletes: vec![], sets: vec![], merges: vec![],
    }
}

// ─── Data load (copy of realworld_bench's load functions; keeping
// them inline so this binary stays a single self-contained file).
// ─────────────────────────────────────────────────────────────────────

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
            entity_id: eid, type_id: TypeId::new(TYPE_REGION),
            tx_id_assert: TxId::new(0), tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PropertyId::new(PROP_NAME), Value::String(format!("Region {i}"))),
                (PropertyId::new(PROP_CODE), Value::String(code.clone())),
            ],
        });
        codes.push(code);
        in_tx += 1;
        if in_tx >= 500 { tx.commit().unwrap(); tx = engine.begin_write(); in_tx = 0; }
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
            entity_id: eid, type_id: TypeId::new(TYPE_CUSTOMER),
            tx_id_assert: TxId::new(0), tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PropertyId::new(PROP_NAME),   Value::String(format!("Customer {i}"))),
                (PropertyId::new(PROP_REGION), Value::String(region.clone())),
            ],
        });
        ids.push(eid);
        in_tx += 1;
        if in_tx >= 500 { tx.commit().unwrap(); tx = engine.begin_write(); in_tx = 0; }
    }
    if in_tx > 0 { tx.commit().unwrap(); }
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
            hyperedge_id: hid, type_id: TypeId::new(TYPE_SALES),
            tx_id_assert: TxId::new(0), tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(ROLE_BUYER), customers[cust_idx])],
            hyperedge_roles: Vec::new(),
            properties: vec![],
        });
        ids.push(hid);
        in_tx += 1;
        if in_tx >= 500 { tx.commit().unwrap(); tx = engine.begin_write(); in_tx = 0; }
    }
    if in_tx > 0 { tx.commit().unwrap(); }
    ids
}

fn lookup_regions_by_code(engine: &mut Engine, codes: &[String]) -> Vec<EntityId> {
    let mut id_by_code: HashMap<String, EntityId> = HashMap::with_capacity(codes.len());
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

fn load_contains_chain(
    engine: &mut Engine,
    region_ids: &[EntityId],
) -> (Vec<EntityId>, Vec<EntityId>) {
    let n = region_ids.len();
    let mut tx = engine.begin_write();
    let mut in_tx = 0;
    let mut roots = Vec::new();
    let mut leaves = Vec::new();
    for i in 0..N_CONTAINS_EDGES {
        let parent_idx = i % n;
        let child_idx = (parent_idx + n / 4 + (i / n) * (n / 8)) % n;
        if parent_idx == child_idx { continue; }
        let parent = region_ids[parent_idx];
        let child = region_ids[child_idx];
        tx.put_hyperedge(HyperEdgeRecord {
            hyperedge_id: HyperedgeId::now_v7(),
            type_id: TypeId::new(TYPE_CONTAINS),
            tx_id_assert: TxId::new(0), tx_id_supersede: TxId::ACTIVE,
            roles: vec![
                (RoleId::new(ROLE_PARENT), parent),
                (RoleId::new(ROLE_CHILD),  child),
            ],
            hyperedge_roles: Vec::new(),
            properties: vec![],
        });
        if i < 4 { roots.push(parent); }
        if i % 7 == 0 { leaves.push(child); }
        in_tx += 1;
        if in_tx >= 500 { tx.commit().unwrap(); tx = engine.begin_write(); in_tx = 0; }
    }
    if in_tx > 0 { tx.commit().unwrap(); }
    (roots, leaves)
}

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
