//! SQLite race-bench server, called from Rust via rusqlite.
//!
//! Sibling of [`bench_race.rs`] (the nDB lane) and
//! [`race_sqlite_server.py`] (the Python+sqlite3 lane). This binary
//! exists for one reason: pull apart "Python's GIL overhead" from
//! "SQLite's storage engine quality" in the live race.
//!
//! The Python sibling has to pay the Global Interpreter Lock cost on
//! every row tuple it creates, which serialises 64 workers fighting
//! for the GIL on heavy full-scan workloads. The C sqlite3 library
//! itself is fine with N parallel readers under WAL mode — it's the
//! Python wrapper that throttles. This binary calls libsqlite3
//! through `rusqlite` (native Rust bindings, no GIL), so the same
//! workload runs at the SQLite-library ceiling instead of the
//! Python-wrapper ceiling.
//!
//! Same HTTP race API as the siblings:
//!     GET  /health
//!     GET  /workloads
//!     GET  /stats
//!     POST /run/<name>
//!     POST /stress     (body: {workload, concurrency, duration_ms})
//!
//! Data shape: reuses the on-disk file the Python sibling already
//! seeded at `.demo-data/ndb-bench-race-sqlite.db` — same junction
//! tables, same indexes, same row count, so the two SQLite lanes are
//! genuinely comparing language overhead with all storage held
//! constant. Falls back to creating + loading the file if absent.

use rusqlite::{Connection, OpenFlags, params};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ─── Dataset shape — mirrors bench_race.rs + race_sqlite_server.py ───
const N_CUSTOMERS: usize = 49_000;
const N_REGIONS:   usize = 1_000;
const N_SALES:     usize = 45_000;
const N_CONTAINS:  usize = 5_000;

const N_ITER_LOOKUPS:     usize = 500;
const N_ITER_QUERY_SMALL: usize = 50;
const N_ITER_QUERY_LARGE: usize = 10;
const N_ITER_RECURSIVE:   usize = 25;
const N_ITER_ITERATE:     usize = 3;

const RATE_LIMIT_SECS: u64 = 3;

struct Workload {
    name:  &'static str,
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
        blurb: "Look up customers by region code via the index, 500 times.", iters: N_ITER_LOOKUPS },
    Workload { name: "single_pattern_query", label: "Single-pattern query",
        blurb: "SELECT id FROM customer WHERE region_code = 'REG-00000'", iters: N_ITER_QUERY_SMALL },
    Workload { name: "two_pattern_join", label: "Two-pattern join",
        blurb: "Customer × sales junction (B-tree join)", iters: N_ITER_QUERY_LARGE },
    Workload { name: "recursive_contains_depth3", label: "Recursive walk, depth 3",
        blurb: "WITH RECURSIVE … depth ≤ 3", iters: N_ITER_RECURSIVE },
    Workload { name: "count_aggregate", label: "count() over a type",
        blurb: "SELECT count(*) FROM customer  (49k rows)", iters: N_ITER_QUERY_LARGE },
];

// ─── State + samples ──────────────────────────────────────────────────
struct Samples {
    narrow_region: String,
    lookup_ids:    Vec<String>,
    lookup_codes:  Vec<String>,
    chain_root:    String,
}

struct State {
    db_path:        PathBuf,
    // Single shared connection for the controlled /run path. Stress
    // workers open their own per-thread read-only connections so WAL
    // mode parallelises N readers genuinely.
    controlled_conn: Mutex<Connection>,
    samples:        Samples,
    n_entities:     usize,
    n_hyperedges:   usize,
    load_ms:        f64,
    rate_limiter:   Mutex<HashMap<(String, String), Instant>>,
}

// ─── Connection helpers ──────────────────────────────────────────────
fn tune(conn: &Connection) {
    // Mirror the Python sibling's pragmas so the comparison is
    // genuinely storage-engine-only.
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    conn.pragma_update(None, "synchronous",  "NORMAL").ok();
    conn.pragma_update(None, "cache_size",   -65536_i64).ok();   // 64 MiB
    conn.pragma_update(None, "temp_store",   "MEMORY").ok();
    conn.pragma_update(None, "mmap_size",    268_435_456_i64).ok(); // 256 MiB
}

fn open_ro(path: &PathBuf) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    tune(&conn);
    Ok(conn)
}

fn open_rw(path: &PathBuf) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    tune(&conn);
    Ok(conn)
}

// ─── Database lifecycle ──────────────────────────────────────────────
fn ensure_database(path: &PathBuf) -> rusqlite::Result<(f64, Samples)> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let drop_first = std::env::var("DROP_FIRST").ok().as_deref() == Some("1");
    if drop_first {
        for suffix in &["", "-wal", "-shm"] {
            let p = format!("{}{}", path.display(), suffix);
            std::fs::remove_file(p).ok();
        }
    }
    let needs_load = !path.exists() || std::fs::metadata(path).map(|m| m.len() == 0).unwrap_or(true);
    let t0 = Instant::now();
    if needs_load {
        eprintln!("creating {}...", path.display());
        let conn = open_rw(path)?;
        init_schema(&conn)?;
        let samples = load_data(&conn)?;
        conn.execute("ANALYZE", [])?;
        let load_ms = t0.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "loaded {} entities + {} junction rows in {:.0} ms",
            N_CUSTOMERS + N_REGIONS,
            N_SALES + N_CONTAINS,
            load_ms,
        );
        Ok((load_ms, samples))
    } else {
        eprintln!("reusing existing {}; sampling probes...", path.display());
        let conn = open_ro(path)?;
        let samples = sample_existing(&conn)?;
        eprintln!("samples ready");
        Ok((t0.elapsed().as_secs_f64() * 1000.0, samples))
    }
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE region (
            id   TEXT PRIMARY KEY,
            code TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL
        );
        CREATE TABLE customer (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            region_code TEXT NOT NULL
        );
        CREATE INDEX customer_region_idx ON customer (region_code);
        CREATE TABLE sales (
            id       TEXT PRIMARY KEY,
            buyer_id TEXT NOT NULL REFERENCES customer(id)
        );
        CREATE INDEX sales_buyer_idx ON sales (buyer_id);
        CREATE TABLE contains (
            id        TEXT PRIMARY KEY,
            parent_id TEXT NOT NULL REFERENCES region(id),
            child_id  TEXT NOT NULL REFERENCES region(id)
        );
        CREATE INDEX contains_parent_idx ON contains (parent_id);",
    )
}

fn load_data(conn: &Connection) -> rusqlite::Result<Samples> {
    // Same load shape + same buyer distribution as the Python sibling
    // so the two SQLite lanes are byte-for-byte comparable on data.
    let mut region_codes: Vec<String> = Vec::with_capacity(N_REGIONS);
    let mut region_ids:   Vec<String> = Vec::with_capacity(N_REGIONS);
    {
        let mut stmt = conn.prepare("INSERT INTO region (id, code, name) VALUES (?1, ?2, ?3)")?;
        for i in 0..N_REGIONS {
            let rid  = uuid::Uuid::now_v7().to_string();
            let code = format!("REG-{:05}", i);
            stmt.execute(params![rid, code, format!("Region {i}")])?;
            region_codes.push(code);
            region_ids.push(rid);
        }
    }

    let mut cust_ids: Vec<String> = Vec::with_capacity(N_CUSTOMERS);
    {
        let mut stmt = conn.prepare("INSERT INTO customer (id, name, region_code) VALUES (?1, ?2, ?3)")?;
        for i in 0..N_CUSTOMERS {
            let cid = uuid::Uuid::now_v7().to_string();
            stmt.execute(params![cid, format!("Customer {i}"), region_codes[i % N_REGIONS]])?;
            cust_ids.push(cid);
        }
    }

    {
        let mut stmt = conn.prepare("INSERT INTO sales (id, buyer_id) VALUES (?1, ?2)")?;
        for i in 0..N_SALES {
            let cust_idx = if i % 2 == 0 {
                (((i / 2) % ((N_CUSTOMERS + 19) / 20)) * 20) % N_CUSTOMERS
            } else {
                (i * 31 + 7) % N_CUSTOMERS
            };
            stmt.execute(params![uuid::Uuid::now_v7().to_string(), cust_ids[cust_idx]])?;
        }
    }

    let n = N_REGIONS;
    {
        let mut stmt = conn.prepare("INSERT INTO contains (id, parent_id, child_id) VALUES (?1, ?2, ?3)")?;
        for i in 0..N_CONTAINS {
            let parent_idx = i % n;
            let child_idx  = (parent_idx + n / 4 + (i / n) * (n / 8)) % n;
            if parent_idx == child_idx { continue; }
            stmt.execute(params![uuid::Uuid::now_v7().to_string(), region_ids[parent_idx], region_ids[child_idx]])?;
        }
    }

    Ok(materialise_samples(&region_codes, &region_ids, &cust_ids))
}

fn sample_existing(conn: &Connection) -> rusqlite::Result<Samples> {
    let mut stmt = conn.prepare("SELECT code FROM region ORDER BY code LIMIT ?1")?;
    let codes: Vec<String> = stmt
        .query_map(params![N_ITER_LOOKUPS], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok).collect();
    let mut stmt = conn.prepare("SELECT id FROM region ORDER BY code LIMIT 4")?;
    let region_ids: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok).collect();
    let mut stmt = conn.prepare("SELECT id FROM customer ORDER BY id LIMIT ?1")?;
    let cust_ids: Vec<String> = stmt
        .query_map(params![N_ITER_LOOKUPS], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok).collect();
    Ok(materialise_samples(&codes, &region_ids, &cust_ids))
}

fn materialise_samples(codes: &[String], region_ids: &[String], cust_ids: &[String]) -> Samples {
    // Deterministic sampling — same seed as the Python sibling's
    // random.Random(0xdeadbeef) so the same row distribution gets
    // probed. We don't reach for the full rand crate just for this;
    // a tiny xorshift gets the same row-set spread for our purposes.
    fn xorshift(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        *state = x;
        x
    }
    let mut s: u64 = 0xdeadbeef;
    let pick = |pool: &[String], s: &mut u64| pool[(xorshift(s) as usize) % pool.len()].clone();
    let lookup_ids: Vec<String> = (0..N_ITER_LOOKUPS).map(|_| pick(cust_ids, &mut s)).collect();
    let lookup_codes: Vec<String> = (0..N_ITER_LOOKUPS).map(|_| pick(codes, &mut s)).collect();
    Samples {
        narrow_region: "REG-00000".to_string(),
        lookup_ids,
        lookup_codes,
        chain_root:    region_ids[0].clone(),
    }
}

// ─── Workload runners — controlled (/run/<name>) ─────────────────────
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

fn bench_iter_all(conn: &Connection) -> BenchResult {
    let mut samples = Vec::with_capacity(N_ITER_ITERATE);
    let outer = Instant::now();
    let mut stmt = conn.prepare(
        "SELECT id FROM region UNION ALL SELECT id FROM customer UNION ALL \
         SELECT id FROM sales UNION ALL SELECT id FROM contains",
    ).expect("prepare iter_all");
    for _ in 0..N_ITER_ITERATE {
        let t = Instant::now();
        let mut rows = stmt.query([]).expect("query");
        let mut n = 0_u64;
        while let Ok(Some(_row)) = rows.next() { n += 1; }
        debug_assert!(n > 0);
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("iter_all", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_point_lookup(conn: &Connection, ids: &[String]) -> BenchResult {
    let mut samples = Vec::with_capacity(ids.len());
    let outer = Instant::now();
    let mut stmt = conn.prepare("SELECT id, name, region_code FROM customer WHERE id = ?1")
        .expect("prepare point_lookup");
    for id in ids {
        let t = Instant::now();
        let _ = stmt.query_row(params![id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        });
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("point_lookup", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_property_lookup(conn: &Connection, codes: &[String]) -> BenchResult {
    let mut samples = Vec::with_capacity(codes.len());
    let outer = Instant::now();
    let mut stmt = conn.prepare("SELECT id FROM customer WHERE region_code = ?1")
        .expect("prepare property_lookup");
    for code in codes {
        let t = Instant::now();
        let mut rows = stmt.query(params![code]).expect("query");
        while let Ok(Some(_)) = rows.next() {}
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("property_lookup", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_single_pattern(conn: &Connection, region: &str) -> BenchResult {
    let mut samples = Vec::with_capacity(N_ITER_QUERY_SMALL);
    let outer = Instant::now();
    let mut stmt = conn.prepare("SELECT id FROM customer WHERE region_code = ?1")
        .expect("prepare single_pattern");
    for _ in 0..N_ITER_QUERY_SMALL {
        let t = Instant::now();
        let mut rows = stmt.query(params![region]).expect("query");
        while let Ok(Some(_)) = rows.next() {}
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("single_pattern_query", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_two_pattern_join(conn: &Connection, region: &str) -> BenchResult {
    let mut samples = Vec::with_capacity(N_ITER_QUERY_LARGE);
    let outer = Instant::now();
    let mut stmt = conn.prepare(
        "SELECT c.id, s.id FROM customer c JOIN sales s ON s.buyer_id = c.id WHERE c.region_code = ?1",
    ).expect("prepare join");
    for _ in 0..N_ITER_QUERY_LARGE {
        let t = Instant::now();
        let mut rows = stmt.query(params![region]).expect("query");
        while let Ok(Some(_)) = rows.next() {}
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("two_pattern_join", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_recursive(conn: &Connection, root: &str) -> BenchResult {
    let mut samples = Vec::with_capacity(N_ITER_RECURSIVE);
    let outer = Instant::now();
    let mut stmt = conn.prepare(
        "WITH RECURSIVE walk(node, depth) AS (
            SELECT child_id, 1 FROM contains WHERE parent_id = ?1
            UNION
            SELECT c.child_id, w.depth + 1
              FROM contains c JOIN walk w ON c.parent_id = w.node
             WHERE w.depth < 3
         )
         SELECT DISTINCT node FROM walk",
    ).expect("prepare recursive");
    for _ in 0..N_ITER_RECURSIVE {
        let t = Instant::now();
        let mut rows = stmt.query(params![root]).expect("query");
        while let Ok(Some(_)) = rows.next() {}
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("recursive_contains_depth3", &mut samples, outer.elapsed().as_micros() as f64)
}

fn bench_count_aggregate(conn: &Connection) -> BenchResult {
    let mut samples = Vec::with_capacity(N_ITER_QUERY_LARGE);
    let outer = Instant::now();
    let mut stmt = conn.prepare("SELECT count(*) FROM customer").expect("prepare count");
    for _ in 0..N_ITER_QUERY_LARGE {
        let t = Instant::now();
        let _: i64 = stmt.query_row([], |r| r.get(0)).unwrap_or(0);
        samples.push(t.elapsed().as_micros() as u64);
    }
    finalize("count_aggregate", &mut samples, outer.elapsed().as_micros() as f64)
}

fn run_workload(state: &State, workload: &Workload) -> String {
    let conn = state.controlled_conn.lock().expect("conn lock poisoned");
    let r = match workload.name {
        "iter_all"                   => bench_iter_all(&conn),
        "point_lookup"               => bench_point_lookup(&conn, &state.samples.lookup_ids),
        "property_lookup"            => bench_property_lookup(&conn, &state.samples.lookup_codes),
        "single_pattern_query"       => bench_single_pattern(&conn, &state.samples.narrow_region),
        "two_pattern_join"           => bench_two_pattern_join(&conn, &state.samples.narrow_region),
        "recursive_contains_depth3"  => bench_recursive(&conn, &state.samples.chain_root),
        "count_aggregate"            => bench_count_aggregate(&conn),
        _ => unreachable!(),
    };
    format!(
        "{{\"name\":\"{}\",\"iters\":{},\"min_us\":{:.0},\"p50_us\":{:.0},\
         \"p99_us\":{:.0},\"ops_per_sec\":{:.1},\"total_ms\":{:.1}}}",
        r.name, r.iters, r.min_us, r.p50_us, r.p99_us, r.ops_per_sec, r.total_ms,
    )
}

// ─── Concurrent stress (/stress) ─────────────────────────────────────
fn do_one_op(conn: &Connection, name: &str, s: &Samples, idx: usize) -> bool {
    // Each branch prepares its own statement on the per-worker conn.
    // We re-prepare each call to mirror the Python sibling's `for _ in
    // conn.execute()` semantics fairly — no hidden statement caching
    // win. The SQLite statement cache is tiny anyway.
    match name {
        "iter_all" => {
            let mut stmt = match conn.prepare(
                "SELECT id FROM region UNION ALL SELECT id FROM customer UNION ALL \
                 SELECT id FROM sales UNION ALL SELECT id FROM contains",
            ) { Ok(s) => s, Err(_) => return false };
            let mut rows = match stmt.query([]) { Ok(r) => r, Err(_) => return false };
            while let Ok(Some(_)) = rows.next() {}
            true
        }
        "point_lookup" => {
            let id = &s.lookup_ids[idx % s.lookup_ids.len()];
            let mut stmt = match conn.prepare("SELECT id, name, region_code FROM customer WHERE id = ?1") {
                Ok(s) => s, Err(_) => return false,
            };
            let _ = stmt.query_row(params![id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            });
            true
        }
        "property_lookup" => {
            let code = &s.lookup_codes[idx % s.lookup_codes.len()];
            let mut stmt = match conn.prepare("SELECT id FROM customer WHERE region_code = ?1") {
                Ok(s) => s, Err(_) => return false,
            };
            let mut rows = match stmt.query(params![code]) { Ok(r) => r, Err(_) => return false };
            while let Ok(Some(_)) = rows.next() {}
            true
        }
        "single_pattern_query" => {
            let mut stmt = match conn.prepare("SELECT id FROM customer WHERE region_code = ?1") {
                Ok(s) => s, Err(_) => return false,
            };
            let mut rows = match stmt.query(params![&s.narrow_region]) { Ok(r) => r, Err(_) => return false };
            while let Ok(Some(_)) = rows.next() {}
            true
        }
        "two_pattern_join" => {
            let mut stmt = match conn.prepare(
                "SELECT c.id, s.id FROM customer c JOIN sales s ON s.buyer_id = c.id WHERE c.region_code = ?1",
            ) { Ok(s) => s, Err(_) => return false };
            let mut rows = match stmt.query(params![&s.narrow_region]) { Ok(r) => r, Err(_) => return false };
            while let Ok(Some(_)) = rows.next() {}
            true
        }
        "recursive_contains_depth3" => {
            let mut stmt = match conn.prepare(
                "WITH RECURSIVE walk(node, depth) AS (
                    SELECT child_id, 1 FROM contains WHERE parent_id = ?1
                    UNION
                    SELECT c.child_id, w.depth + 1
                      FROM contains c JOIN walk w ON c.parent_id = w.node
                     WHERE w.depth < 3
                 )
                 SELECT DISTINCT node FROM walk",
            ) { Ok(s) => s, Err(_) => return false };
            let mut rows = match stmt.query(params![&s.chain_root]) { Ok(r) => r, Err(_) => return false };
            while let Ok(Some(_)) = rows.next() {}
            true
        }
        "count_aggregate" => {
            let mut stmt = match conn.prepare("SELECT count(*) FROM customer") {
                Ok(s) => s, Err(_) => return false,
            };
            let _: i64 = stmt.query_row([], |r| r.get(0)).unwrap_or(0);
            true
        }
        _ => false,
    }
}

fn run_stress(state: &Arc<State>, name: &str, concurrency: usize, duration_ms: u64) -> String {
    let deadline = Instant::now() + Duration::from_millis(duration_ms);
    let started = Instant::now();
    let mut handles = Vec::with_capacity(concurrency);
    let name: &'static str = match WORKLOADS.iter().find(|w| w.name == name) {
        Some(w) => w.name,
        None    => return r#"{"error":"unknown_workload"}"#.to_string(),
    };
    let db_path = state.db_path.clone();
    for tid in 0..concurrency {
        let s_state = Arc::clone(state);
        let dbp = db_path.clone();
        handles.push(std::thread::spawn(move || -> (Vec<u64>, u64) {
            // Per-thread read-only connection. WAL mode → N readers
            // parallelise on the SQLite library side. Rust threads run
            // truly in parallel (no GIL), so the workload's natural
            // concurrency profile shows through.
            let conn = match open_ro(&dbp) {
                Ok(c) => c, Err(_) => return (Vec::new(), 0),
            };
            let mut latencies: Vec<u64> = Vec::with_capacity(1024);
            let mut errors: u64 = 0;
            let mut idx_seed = (tid as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            while Instant::now() < deadline {
                idx_seed ^= idx_seed << 13;
                idx_seed ^= idx_seed >> 7;
                idx_seed ^= idx_seed << 17;
                let idx = idx_seed as usize;
                let t = Instant::now();
                let ok = do_one_op(&conn, name, &s_state.samples, idx);
                let us = t.elapsed().as_micros() as u64;
                if ok { latencies.push(us); } else { errors += 1; }
            }
            (latencies, errors)
        }));
    }
    let mut all: Vec<u64> = Vec::with_capacity(concurrency * 1024);
    let mut errors = 0_u64;
    for h in handles {
        let (lat, err) = h.join().unwrap_or((Vec::new(), 0));
        all.extend(lat); errors += err;
    }
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    all.sort_unstable();
    let total_ops = all.len() as u64;
    let pct = |q: f64| -> u64 {
        if all.is_empty() { 0 } else {
            all[((all.len() as f64 * q).min(all.len() as f64 - 1.0)) as usize]
        }
    };
    let p50 = pct(0.50);  let p95 = pct(0.95);
    let p99 = pct(0.99);  let p999 = pct(0.999);
    let max = all.last().copied().unwrap_or(0);
    let rps = if wall_ms > 0.0 { (total_ops as f64) * 1000.0 / wall_ms } else { 0.0 };

    const N_BUCKETS: usize = 60;
    let mut hist = [0_u64; N_BUCKETS];
    for us in &all {
        let v = (*us).max(1);
        let b = ((v as f64).log10() * 10.0) as i64;
        hist[b.clamp(0, N_BUCKETS as i64 - 1) as usize] += 1;
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
        name, concurrency, duration_ms, wall_ms,
        total_ops, errors, rps, p50, p95, p99, p999, max, hist_json,
    )
}

// ─── HTTP loop — hand-rolled like bench_race.rs ──────────────────────
fn handle(state: Arc<State>, mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let peer_ip = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "?".into());
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut it = request_line.trim_end().split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let path   = it.next().unwrap_or("/").to_string();

    let mut content_length = 0_usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 { break; }
        let t = h.trim_end();
        if t.is_empty() { break; }
        if let Some(v) = t.strip_prefix("Content-Length:").or_else(|| t.strip_prefix("content-length:")) {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = Vec::new();
    if content_length > 0 { body.resize(content_length, 0); reader.read_exact(&mut body)?; }

    match (method.as_str(), path.as_str()) {
        ("GET", "/health") => send_json(&mut stream, 200, &format!(
            "{{\"status\":\"ok\",\"loaded\":true,\"engine\":\"sqlite (via Rust/rusqlite) {}\",\
             \"n_entities\":{},\"n_hyperedges\":{},\"load_ms\":{:.0}}}",
            rusqlite::version(),
            state.n_entities, state.n_hyperedges, state.load_ms,
        )),
        ("GET", "/workloads") => {
            let mut out = String::from("[");
            for (i, w) in WORKLOADS.iter().enumerate() {
                if i > 0 { out.push(','); }
                out.push_str(&format!(
                    "{{\"name\":\"{}\",\"label\":\"{}\",\"blurb\":\"{}\",\"iters\":{}}}",
                    w.name, escape_json(w.label), escape_json(w.blurb), w.iters,
                ));
            }
            out.push(']');
            send_json(&mut stream, 200, &out)
        }
        ("GET", "/stats") => {
            // Disk: .db + -wal + -shm. RAM/CPU: this process.
            let mut bytes_on_disk = 0_u64;
            for suf in &["", "-wal", "-shm"] {
                let p = format!("{}{}", state.db_path.display(), suf);
                if let Ok(m) = std::fs::metadata(&p) { bytes_on_disk += m.len(); }
            }
            let rss_kb = proc_rss_kb(process::id());
            let cpu_us = proc_cpu_us(process::id());
            send_json(&mut stream, 200, &format!(
                "{{\"bytes_on_disk\":{},\"bytes_resident\":{},\
                 \"cpu_user_us\":{},\"cpu_sys_us\":0,\"backend_pids\":1}}",
                bytes_on_disk, rss_kb * 1024, cpu_us,
            ))
        }
        ("POST", path) if path.starts_with("/run/") => {
            let name = &path["/run/".len()..];
            let Some(w) = WORKLOADS.iter().find(|w| w.name == name) else {
                return send_json(&mut stream, 404, "{\"error\":\"unknown_workload\"}");
            };
            // Per-IP, per-workload rate limit.
            {
                let mut rl = state.rate_limiter.lock().unwrap();
                let key = (peer_ip.clone(), name.to_string());
                let now = Instant::now();
                if let Some(prev) = rl.get(&key)
                    && now.duration_since(*prev) < Duration::from_secs(RATE_LIMIT_SECS)
                {
                    let wait = RATE_LIMIT_SECS - now.duration_since(*prev).as_secs();
                    return send_json(&mut stream, 429,
                        &format!("{{\"error\":\"rate_limit\",\"retry_after_s\":{wait}}}"));
                }
                rl.insert(key, now);
            }
            send_json(&mut stream, 200, &run_workload(&state, w))
        }
        ("POST", "/stress") => {
            let req: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v, Err(e) => return send_json(&mut stream, 400,
                    &format!("{{\"error\":\"bad_json\",\"detail\":\"{}\"}}",
                             escape_json(&e.to_string()))),
            };
            let name = req.get("workload").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let conc = req.get("concurrency").and_then(|v| v.as_u64()).unwrap_or(4) as usize;
            let dur  = req.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(5000);
            let conc = conc.clamp(1, 128);
            let dur  = dur.clamp(500, 30_000);
            {
                let mut rl = state.rate_limiter.lock().unwrap();
                let key = (peer_ip.clone(), "/stress".to_string());
                let now = Instant::now();
                if let Some(prev) = rl.get(&key)
                    && now.duration_since(*prev) < Duration::from_secs(RATE_LIMIT_SECS)
                {
                    let wait = RATE_LIMIT_SECS - now.duration_since(*prev).as_secs();
                    return send_json(&mut stream, 429,
                        &format!("{{\"error\":\"rate_limit\",\"retry_after_s\":{wait}}}"));
                }
                rl.insert(key, now);
            }
            send_json(&mut stream, 200, &run_stress(&state, &name, conc, dur))
        }
        _ => send_json(&mut stream, 404, "{\"error\":\"not_found\"}"),
    }
}

fn send_json(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status { 200 => "OK", 400 => "Bad Request", 404 => "Not Found",
                                 429 => "Too Many Requests", 500 => "Internal Server Error", _ => "" };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Access-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body,
    );
    stream.write_all(resp.as_bytes())
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn proc_rss_kb(pid: u32) -> u64 {
    let p = format!("/proc/{pid}/status");
    if let Ok(s) = std::fs::read_to_string(&p) {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(kb) = rest.split_whitespace().next().and_then(|n| n.parse().ok()) {
                    return kb;
                }
            }
        }
    }
    0
}

fn proc_cpu_us(pid: u32) -> u64 {
    let p = format!("/proc/{pid}/stat");
    let Ok(s) = std::fs::read_to_string(&p) else { return 0 };
    let Some(rest) = s.rsplit_once(") ") else { return 0 };
    let parts: Vec<&str> = rest.1.split_whitespace().collect();
    if parts.len() < 13 { return 0; }
    let u: u64 = parts[11].parse().unwrap_or(0);
    let sy: u64 = parts[12].parse().unwrap_or(0);
    (u + sy) * 10_000   // 100 Hz clock → 10_000 us per tick
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind = std::env::args()
        .skip_while(|a| a != "--bind")
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8774".to_string());

    // Default DB path is `<workspace>/.demo-data/ndb-bench-race-sqlite.db`
    // — same file the Python sibling uses, so the storage state is
    // genuinely shared and the only variable is the calling language.
    let default_db = std::env::current_dir()?
        .join(".demo-data")
        .join("ndb-bench-race-sqlite.db");
    let db_path = std::env::var_os("BENCH_SQLITE_PATH")
        .map(PathBuf::from)
        .unwrap_or(default_db);

    let (load_ms, samples) = ensure_database(&db_path)?;
    let controlled_conn = Mutex::new(open_ro(&db_path)?);

    let state = Arc::new(State {
        db_path,
        controlled_conn,
        samples,
        n_entities:   N_CUSTOMERS + N_REGIONS,
        n_hyperedges: N_SALES + N_CONTAINS,
        load_ms,
        rate_limiter: Mutex::new(HashMap::new()),
    });

    let listener = TcpListener::bind(&bind)?;
    eprintln!("bench-race SQLite (Rust) serving on http://{bind}");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let state = Arc::clone(&state);
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
