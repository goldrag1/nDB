//! seismic_ndb — the third nDB science demo.
//!
//! Seeds ~4,800 earthquakes from a USGS 30-day live snapshot plus
//! ~13 curated historic mega-quake aftershock windows (1989 Loma
//! Prieta through 2025 Myanmar), 14 major fault systems, and the
//! ON_FAULT + AFTERSHOCK_SEQUENCE hyperedges that wire them.
//!
//! - `127.0.0.1:8746` — ndb-server for /iter and /commit
//! - `127.0.0.1:9878` — static file server for docs/seismic/
//!
//! Mirror of v22_explorer / exoplanet_explorer — same architecture so
//! the knowledge-site proxy treats all demos uniformly.
//!
//! Run:
//! ```sh
//! cargo run -p ndb-renderer --example seismic_explorer
//! ```
//! Then open <http://127.0.0.1:9878/> or, through the tunnel,
//! <https://ndb.nextstar-erp.com/seismic_ndb/>.
#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ndb_engine::Engine;
use ndb_engine::id::{EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId};
use ndb_engine::record::{EntityRecord, HyperEdgeRecord, Record};
use ndb_engine::value::Value;
use ndb_server::Server;
use serde::Deserialize;

// ─── Schema (in lockstep with docs/seismic/index.html) ──────────────
const T_EARTHQUAKE: u32 = 1;
const T_FAULT: u32 = 2;
const T_AGENCY: u32 = 3;

const T_AFTERSHOCK_SEQUENCE: u32 = 100; // arity (N+1): [mainshock, aftershock_1..N]
const T_ON_FAULT: u32 = 101;            // arity 2: [event, fault]
const T_HISTORIC_EVENT: u32 = 102;      // arity 1: [event] — marker for curated historic mainshocks

const ROLE_MAINSHOCK: u32 = 10;
const ROLE_AFTERSHOCK: u32 = 11;
const ROLE_EVENT: u32 = 12;
const ROLE_FAULT: u32 = 13;

const PROP_NAME: u32 = 30;
// Earthquake props
const PROP_USGS_ID: u32 = 31;
const PROP_MAGNITUDE: u32 = 32;
const PROP_MAG_TYPE: u32 = 33;
const PROP_PLACE: u32 = 34;
const PROP_TIME_MS: u32 = 35;
const PROP_LAT: u32 = 36;
const PROP_LON: u32 = 37;
const PROP_DEPTH_KM: u32 = 38;
const PROP_TSUNAMI: u32 = 39;
const PROP_FELT: u32 = 40;
const PROP_SIG: u32 = 41;
const PROP_NET: u32 = 42;
const PROP_ALERT: u32 = 43;
const PROP_SOURCE: u32 = 44;       // "live30d" or "historic:<slug>"
// Fault props
const PROP_FAULT_TYPE: u32 = 50;
const PROP_FAULT_COUNTRY: u32 = 51;
const PROP_FAULT_TRACE_JSON: u32 = 52;   // serialised lat/lon polyline
// On-fault edge props
const PROP_FAULT_DISTANCE_KM: u32 = 55;
// Sequence edge props
const PROP_SEQ_WINDOW_DAYS: u32 = 60;
const PROP_SEQ_RADIUS_KM: u32 = 61;
const PROP_SEQ_HISTORIC: u32 = 62;
// Agency props
const PROP_AGENCY_CODE: u32 = 70;

const API_PORT: u16 = 8746;
const STATIC_PORT: u16 = 9878;

// Seed JSON baked into the binary at build time. ~1.5 MB; only parsed
// on first launch when the engine has zero earthquake entities.
const SEED_JSON: &str = include_str!("seed.json");

// ─── Seed JSON shape ────────────────────────────────────────────────

#[derive(Deserialize)]
struct SeedDoc {
    events:             Vec<EventRow>,
    agencies:           Vec<AgencyRow>,
    faults:             Vec<FaultRow>,
    live_sequences:     Vec<SeqRow>,
    historic_sequences: Vec<SeqRow>,
    on_fault:           Vec<OnFaultRow>,
}

#[derive(Deserialize)]
struct EventRow {
    id:       String,
    mag:      Option<f64>,
    mag_type: Option<String>,
    place:    Option<String>,
    time_ms:  Option<i64>,
    tsunami:  Option<bool>,
    felt:     Option<i64>,
    sig:      Option<i64>,
    net:      Option<String>,
    alert:    Option<String>,
    lat:      f64,
    lon:      f64,
    depth_km: Option<f64>,
    source:   String,
}

#[derive(Deserialize)]
struct AgencyRow {
    short: String,
    code:  String,
}

#[derive(Deserialize)]
struct FaultRow {
    name:    String,
    #[serde(rename = "type")]
    kind:    String,
    country: String,
    trace:   Vec<(f64, f64)>,
}

#[derive(Deserialize)]
struct SeqRow {
    mainshock_id:   String,
    aftershock_ids: Vec<String>,
    name:           String,
    window_days:    i64,
    radius_km:      i64,
    #[serde(default)]
    historic:       bool,
}

#[derive(Deserialize)]
struct OnFaultRow {
    event_id:    String,
    fault_name:  String,
    distance_km: f64,
}

// ─── Main ──────────────────────────────────────────────────────────

fn main() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let db_dir_owned = workspace_root.join(".demo-data/seismic-ndb");
    let db_dir = db_dir_owned.as_path();
    if !db_dir.exists() {
        std::fs::create_dir_all(db_dir).expect("mkdir db");
    }
    let mut engine = if Engine::open(db_dir).is_ok() {
        Engine::open(db_dir).expect("open engine")
    } else {
        Engine::create(db_dir).expect("create engine")
    };

    let existing_events = count_entities_of_type(&engine, T_EARTHQUAKE);
    if existing_events == 0 {
        eprintln!("first run — seeding seismic_ndb from USGS snapshot");
        seed(&mut engine);
        engine.flush().expect("flush");
    } else {
        eprintln!(
            "reusing existing nDB at {} ({} earthquake entities already stored)",
            db_dir.display(),
            existing_events,
        );
    }
    drop(engine);

    // ndb-server — public demo, read-only via the wire. The SPA's
    // USGS live-feed poller still works in-process via the launcher
    // helper, but a visitor cannot mutate state.
    let server = Arc::new(
        Server::open(db_dir)
            .expect("server open")
            .with_cors_origin("*")
            .with_read_only(true),
    );
    let api_addr = format!("127.0.0.1:{API_PORT}");
    let api_listener = TcpListener::bind(&api_addr).expect("bind ndb-server");
    eprintln!("ndb-server  listening on http://{api_addr}");
    let api_server = Arc::clone(&server);
    std::thread::spawn(move || {
        std::thread::scope(|s| {
            for stream in api_listener.incoming() {
                let Ok(stream) = stream else { continue };
                let me = Arc::clone(&api_server);
                s.spawn(move || {
                    if let Err(e) = me.handle_connection(stream) {
                        eprintln!("ndb-server connection: {e}");
                    }
                });
            }
        });
    });

    // Static
    let static_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("docs/seismic");
    let static_addr = format!("127.0.0.1:{STATIC_PORT}");
    let static_listener = TcpListener::bind(&static_addr).expect("bind static");
    eprintln!("explorer    listening on http://{static_addr}");
    eprintln!();
    eprintln!("    Open http://127.0.0.1:{STATIC_PORT}/  in your browser");
    eprintln!("    Press Ctrl-C to stop both servers");
    eprintln!();
    let root = static_root.clone();
    std::thread::spawn(move || {
        for stream in static_listener.incoming() {
            let Ok(stream) = stream else { continue };
            let root = root.clone();
            std::thread::spawn(move || {
                if let Err(e) = serve_static(stream, &root) {
                    eprintln!("static connection: {e}");
                }
            });
        }
    });

    loop { std::thread::park(); }
}

// ─── Seed ──────────────────────────────────────────────────────────

fn seed(engine: &mut Engine) {
    let doc: SeedDoc = serde_json::from_str(SEED_JSON).expect("parse seed.json");

    let mut event_ids:  HashMap<String, EntityId> = HashMap::with_capacity(doc.events.len());
    let mut fault_ids:  HashMap<String, EntityId> = HashMap::with_capacity(doc.faults.len());
    let mut agency_ids: HashMap<String, EntityId> = HashMap::with_capacity(doc.agencies.len());

    // ── Earthquakes — batched commits (500 per txn) so we don't open
    //    one transaction per record (slow) nor one for all 4800 (memory).
    const BATCH: usize = 500;
    let mut chunk: Vec<(EntityId, Vec<(u32, Value)>)> = Vec::with_capacity(BATCH);
    let total = doc.events.len();
    let mut done = 0usize;
    for e in &doc.events {
        let eid = EntityId::now_v7();
        let mut props: Vec<(u32, Value)> = Vec::with_capacity(14);
        props.push((PROP_USGS_ID, Value::String(e.id.clone())));
        props.push((PROP_NAME,    Value::String(
            e.place.clone().unwrap_or_else(|| e.id.clone()))));
        if let Some(v) = e.mag        { props.push((PROP_MAGNITUDE, Value::F64(v))); }
        if let Some(v) = &e.mag_type  { props.push((PROP_MAG_TYPE,  Value::String(v.clone()))); }
        if let Some(v) = &e.place     { props.push((PROP_PLACE,     Value::String(v.clone()))); }
        if let Some(v) = e.time_ms    { props.push((PROP_TIME_MS,   Value::I64(v))); }
        props.push((PROP_LAT, Value::F64(e.lat)));
        props.push((PROP_LON, Value::F64(e.lon)));
        if let Some(v) = e.depth_km   { props.push((PROP_DEPTH_KM,  Value::F64(v))); }
        if let Some(v) = e.tsunami    { props.push((PROP_TSUNAMI,   Value::String(
            if v { "yes".into() } else { "no".into() }))); }
        if let Some(v) = e.felt       { props.push((PROP_FELT,      Value::I64(v))); }
        if let Some(v) = e.sig        { props.push((PROP_SIG,       Value::I64(v))); }
        if let Some(v) = &e.net       { props.push((PROP_NET,       Value::String(v.clone()))); }
        if let Some(v) = &e.alert     { props.push((PROP_ALERT,     Value::String(v.clone()))); }
        props.push((PROP_SOURCE, Value::String(e.source.clone())));
        event_ids.insert(e.id.clone(), eid);
        chunk.push((eid, props));
        if chunk.len() >= BATCH {
            commit_entities_batch(engine, T_EARTHQUAKE, std::mem::take(&mut chunk));
            done += BATCH;
            if done % 1000 == 0 || done == total {
                eprintln!("  seeded {done}/{total} earthquakes");
            }
        }
    }
    if !chunk.is_empty() {
        let n = chunk.len();
        commit_entities_batch(engine, T_EARTHQUAKE, chunk);
        done += n;
        eprintln!("  seeded {done}/{total} earthquakes");
    }

    // ── Agencies
    for a in &doc.agencies {
        let eid = EntityId::now_v7();
        commit_entity(engine, eid, T_AGENCY, vec![
            (PROP_NAME,        Value::String(a.short.clone())),
            (PROP_AGENCY_CODE, Value::String(a.code.clone())),
        ]);
        agency_ids.insert(a.code.clone(), eid);
    }

    // ── Faults — serialise the trace as JSON so the SPA can draw it without
    //    a separate fetch. nDB doesn't have a polyline type; JSON-as-string
    //    is the honest pragmatic choice.
    for f in &doc.faults {
        let eid = EntityId::now_v7();
        let trace_json = serde_json::to_string(&f.trace).unwrap_or_else(|_| "[]".into());
        commit_entity(engine, eid, T_FAULT, vec![
            (PROP_NAME,             Value::String(f.name.clone())),
            (PROP_FAULT_TYPE,       Value::String(f.kind.clone())),
            (PROP_FAULT_COUNTRY,    Value::String(f.country.clone())),
            (PROP_FAULT_TRACE_JSON, Value::String(trace_json)),
        ]);
        fault_ids.insert(f.name.clone(), eid);
    }

    // ── Aftershock sequences
    let total_seqs = doc.live_sequences.len() + doc.historic_sequences.len();
    let mut seq_skipped = 0;
    for seq in doc.live_sequences.iter().chain(doc.historic_sequences.iter()) {
        let Some(ms) = event_ids.get(&seq.mainshock_id) else {
            seq_skipped += 1;
            continue;
        };
        let mut roles = vec![(RoleId::new(ROLE_MAINSHOCK), *ms)];
        for aid in &seq.aftershock_ids {
            if let Some(after) = event_ids.get(aid) {
                roles.push((RoleId::new(ROLE_AFTERSHOCK), *after));
            }
        }
        if roles.len() < 2 { continue; }
        let props = vec![
            (PROP_NAME,             Value::String(seq.name.clone())),
            (PROP_SEQ_WINDOW_DAYS,  Value::I64(seq.window_days)),
            (PROP_SEQ_RADIUS_KM,    Value::I64(seq.radius_km)),
            (PROP_SEQ_HISTORIC,     Value::String(
                if seq.historic { "yes".into() } else { "no".into() })),
        ];
        commit_hyperedge(engine, T_AFTERSHOCK_SEQUENCE, roles, props);
    }

    // ── HISTORIC_EVENT marker hyperedges — arity 1, so the SPA can list
    //    "show me only mega-quakes" without trawling every entity.
    for seq in &doc.historic_sequences {
        if let Some(ms) = event_ids.get(&seq.mainshock_id) {
            commit_hyperedge(
                engine, T_HISTORIC_EVENT,
                vec![(RoleId::new(ROLE_EVENT), *ms)],
                vec![(PROP_NAME, Value::String(seq.name.clone()))],
            );
        }
    }

    // ── ON_FAULT
    let mut on_fault_skipped = 0;
    for of in &doc.on_fault {
        let event = match event_ids.get(&of.event_id) {
            Some(e) => *e,
            None => { on_fault_skipped += 1; continue; }
        };
        let fault = match fault_ids.get(&of.fault_name) {
            Some(f) => *f,
            None => continue,
        };
        commit_hyperedge(engine, T_ON_FAULT,
            vec![(RoleId::new(ROLE_EVENT), event), (RoleId::new(ROLE_FAULT), fault)],
            vec![(PROP_FAULT_DISTANCE_KM, Value::F64(of.distance_km))],
        );
    }

    eprintln!(
        "seeded {} events, {} agencies, {} faults, {} sequences ({} historic), {} on-fault links",
        doc.events.len(),
        doc.agencies.len(),
        doc.faults.len(),
        total_seqs - seq_skipped,
        doc.historic_sequences.len(),
        doc.on_fault.len() - on_fault_skipped,
    );
    let max_arity = doc.live_sequences.iter()
        .chain(doc.historic_sequences.iter())
        .map(|s| s.aftershock_ids.len() + 1)
        .max()
        .unwrap_or(0);
    eprintln!("max aftershock-sequence arity: {max_arity}");
}

// ─── Engine helpers ────────────────────────────────────────────────

fn count_entities_of_type(engine: &Engine, type_id: u32) -> usize {
    let mut n = 0_usize;
    for r in engine.snapshot_iter_streaming(TxId::ACTIVE).flatten() {
        if let Record::Entity(e) = r
            && e.type_id == TypeId::new(type_id)
        { n += 1; }
    }
    n
}

fn commit_entity(
    engine: &mut Engine, eid: EntityId, type_id: u32,
    properties: Vec<(u32, Value)>,
) {
    let mut txn = engine.begin_write();
    let tx_id = txn.tx_id();
    txn.put_entity(EntityRecord {
        entity_id: eid,
        type_id: TypeId::new(type_id),
        tx_id_assert: tx_id,
        tx_id_supersede: TxId::ACTIVE,
        properties: properties.into_iter().map(|(p, v)| (PropertyId::new(p), v)).collect(),
    });
    txn.commit().expect("commit entity");
}

fn commit_entities_batch(
    engine: &mut Engine, type_id: u32,
    items: Vec<(EntityId, Vec<(u32, Value)>)>,
) {
    let mut txn = engine.begin_write();
    let tx_id = txn.tx_id();
    for (eid, properties) in items {
        txn.put_entity(EntityRecord {
            entity_id: eid,
            type_id: TypeId::new(type_id),
            tx_id_assert: tx_id,
            tx_id_supersede: TxId::ACTIVE,
            properties: properties.into_iter().map(|(p, v)| (PropertyId::new(p), v)).collect(),
        });
    }
    txn.commit().expect("commit entity batch");
}

fn commit_hyperedge(
    engine: &mut Engine, type_id: u32,
    roles: Vec<(RoleId, EntityId)>,
    properties: Vec<(u32, Value)>,
) {
    let mut txn = engine.begin_write();
    let tx_id = txn.tx_id();
    txn.put_hyperedge(HyperEdgeRecord {
        hyperedge_id: HyperedgeId::now_v7(),
        type_id: TypeId::new(type_id),
        tx_id_assert: tx_id,
        tx_id_supersede: TxId::ACTIVE,
        roles,
        hyperedge_roles: Vec::new(),
        properties: properties.into_iter().map(|(p, v)| (PropertyId::new(p), v)).collect(),
    });
    txn.commit().expect("commit hyperedge");
}

// ─── Static file server (same as v22_explorer / exoplanet_explorer) ─

fn serve_static(stream: TcpStream, root: &Path) -> std::io::Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut header = String::new();
    loop {
        header.clear();
        let n = reader.read_line(&mut header)?;
        if n <= 2 { break; }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/")
        .split('?').next().unwrap_or("/").to_string();
    let mut writer = &stream;
    match resolve_path(root, &path) {
        Some(p) => {
            if let Ok(bytes) = std::fs::read(&p) {
                let ctype = content_type(&p);
                write_response(&mut writer, 200, ctype, &bytes)?;
            } else {
                write_response(&mut writer, 404, "text/plain", b"not found")?;
            }
        }
        None => { write_response(&mut writer, 404, "text/plain", b"not found")?; }
    }
    Ok(())
}

fn resolve_path(root: &Path, req_path: &str) -> Option<PathBuf> {
    if req_path.contains("..") { return None; }
    let trimmed = req_path.trim_start_matches('/');
    let candidate = if trimmed.is_empty() { root.join("index.html") } else { root.join(trimmed) };
    if candidate.is_file() { Some(candidate) } else { None }
}

fn content_type(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js")   => "application/javascript; charset=utf-8",
        Some("css")  => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg")  => "image/svg+xml",
        Some("png")  => "image/png",
        Some("ico")  => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn write_response<W: Write>(w: &mut W, status: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status { 200 => "OK", 404 => "Not Found", _ => "Status" };
    write!(w, "HTTP/1.1 {status} {reason}\r\n")?;
    write!(w, "Content-Type: {ctype}\r\n")?;
    write!(w, "Content-Length: {}\r\n", body.len())?;
    write!(w, "Connection: close\r\n")?;
    write!(w, "Cache-Control: no-cache\r\n")?;
    w.write_all(b"\r\n")?;
    w.write_all(body)?;
    w.flush()?;
    Ok(())
}
