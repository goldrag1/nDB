//! exoplanet_ndb — the second nDB science demo.
//!
//! Seeds 91 confirmed exoplanets across all 11 NASA-recognised detection
//! methods (transit, radial velocity, microlensing, direct imaging,
//! astrometry, pulsar timing, TTV, ETV, OBM, pulsation timing variations,
//! disk kinematics) into ~40 stellar systems. Same architectural shape
//! as `v22_explorer`: an `ndb-server` on the wire-protocol port and a
//! tiny static-file server next to it.
//!
//! - `127.0.0.1:8745` — `ndb-server` (with CORS) for the SPA's
//!   `/iter` and `/commit` calls. Separate port + DB from the alphafold
//!   demo (which uses 8742); no cross-contamination of schemas.
//! - `127.0.0.1:9877` — static file server backing `docs/exoplanet/`.
//!
//! Run from the repo root:
//!
//! ```sh
//! cargo run -p ndb-renderer --example exoplanet_explorer
//! ```
//!
//! Then open <http://127.0.0.1:9877/>. Knowledge-site users reach the
//! same SPA at <https://ndb.nextstar-erp.com/exoplanet_ndb/>, proxied
//! through the Python reverse-proxy on :9880.
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

// ─── Schema constants (must stay in lockstep with docs/exoplanet/index.html) ─
const T_STAR: u32 = 1;
const T_PLANET: u32 = 2;
const T_METHOD: u32 = 3;
const T_MISSION: u32 = 4;

// Hyperedge types. Same numbering convention as v22_explorer: low entity
// types, ≥100 hyperedge types.
const T_DISCOVERY: u32 = 100;       // arity-4: [star, planet, method, mission]
const T_SYSTEM: u32 = 101;          // arity-(N+1): [star, planet_1, …, planet_N]
const T_HABITABLE_ZONE: u32 = 102;  // arity-(M+1): [star, hz_planet_1, …]

const ROLE_STAR: u32 = 10;
const ROLE_PLANET: u32 = 11;
const ROLE_METHOD: u32 = 12;
const ROLE_MISSION: u32 = 13;
const ROLE_HZ_PLANET: u32 = 14;

const PROP_NAME: u32 = 30;
// Star metadata
const PROP_STAR_RA: u32 = 31;
const PROP_STAR_DEC: u32 = 32;
const PROP_STAR_DIST_PC: u32 = 33;
const PROP_STAR_TEFF: u32 = 34;
const PROP_STAR_MASS_MSUN: u32 = 35;
const PROP_STAR_RADIUS_RSUN: u32 = 36;
const PROP_STAR_AGE_GYR: u32 = 37;
// Planet metadata
const PROP_PL_RADIUS_RE: u32 = 40;
const PROP_PL_MASS_ME: u32 = 41;
const PROP_PL_ORBITAL_PERIOD: u32 = 42;
const PROP_PL_SEMI_MAJOR_AU: u32 = 43;
const PROP_PL_EQ_TEMP_K: u32 = 44;
const PROP_PL_HABITABLE: u32 = 45;
const PROP_PL_HOST_NAME: u32 = 46;
// Method / mission metadata
const PROP_MISSION_FULL_NAME: u32 = 50;
// Discovery hyperedge metadata
const PROP_DISC_YEAR: u32 = 60;
const PROP_DISC_FACILITY: u32 = 61;
const PROP_DISC_TELESCOPE: u32 = 62;
// System / HZ hyperedge metadata
const PROP_SYSTEM_HOST: u32 = 70;
const PROP_PLANET_COUNT: u32 = 71;

const API_PORT: u16 = 8745;
const STATIC_PORT: u16 = 9877;

// ─── Seed JSON shape (must match tools/exoplanet/build_seed.py output) ───
const SEED_JSON: &str = include_str!("seed.json");

#[derive(Deserialize)]
struct SeedDoc {
    stars: Vec<StarRow>,
    methods: Vec<MethodRow>,
    missions: Vec<MissionRow>,
    planets: Vec<PlanetRow>,
    discoveries: Vec<DiscoveryRow>,
    systems: Vec<SystemRow>,
    hz_groups: Vec<HzRow>,
}

#[derive(Deserialize)]
struct StarRow {
    name: String,
    ra: Option<f64>,
    dec: Option<f64>,
    distance_pc: Option<f64>,
    teff_k: Option<f64>,
    mass_msun: Option<f64>,
    radius_rsun: Option<f64>,
    age_gyr: Option<f64>,
}

#[derive(Deserialize)]
struct MethodRow {
    name: String,
}

#[derive(Deserialize)]
struct MissionRow {
    short: String,
    full: String,
}

#[derive(Deserialize)]
struct PlanetRow {
    name: String,
    host: String,
    radius_re: Option<f64>,
    mass_me: Option<f64>,
    orbital_period: Option<f64>,
    semi_major_au: Option<f64>,
    eq_temp_k: Option<f64>,
    habitable: bool,
}

#[derive(Deserialize)]
struct DiscoveryRow {
    planet: String,
    star: String,
    method: String,
    mission: String,
    year: Option<i64>,
    facility: String,
    telescope: String,
}

#[derive(Deserialize)]
struct SystemRow {
    host: String,
    planets: Vec<String>,
    n: usize,
}

#[derive(Deserialize)]
struct HzRow {
    host: String,
    planets: Vec<String>,
}

// ─── Main ─────────────────────────────────────────────────────────────

fn main() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let db_dir_owned = workspace_root.join(".demo-data/exoplanet-ndb");
    let db_dir = db_dir_owned.as_path();
    if !db_dir.exists() {
        std::fs::create_dir_all(db_dir).expect("mkdir db");
    }
    let mut engine = if Engine::open(db_dir).is_ok() {
        Engine::open(db_dir).expect("open engine")
    } else {
        Engine::create(db_dir).expect("create engine")
    };

    let existing_planets = count_entities_of_type(&engine, T_PLANET);
    if existing_planets == 0 {
        eprintln!("first run — seeding curated exoplanets from NASA Exoplanet Archive snapshot");
        seed(&mut engine);
        engine.flush().expect("flush");
    } else {
        eprintln!(
            "reusing existing nDB at {} ({} planets already stored; user-added planets preserved)",
            db_dir.display(),
            existing_planets,
        );
    }
    drop(engine);

    // ─── ndb-server (port 8745) ─────────────────────────────────────
    let server = Arc::new(
        Server::open(db_dir)
            .expect("server open")
            .with_cors_origin("*"),
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

    // ─── Static file server (port 9877) ─────────────────────────────
    let static_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("docs/exoplanet");
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

    loop {
        std::thread::park();
    }
}

// ─── Seed ─────────────────────────────────────────────────────────────

fn seed(engine: &mut Engine) {
    let doc: SeedDoc = serde_json::from_str(SEED_JSON).expect("parse seed.json");

    // ID maps so we can resolve names → EntityId when building hyperedges.
    let mut star_ids: HashMap<String, EntityId> = HashMap::new();
    let mut planet_ids: HashMap<String, EntityId> = HashMap::new();
    let mut method_ids: HashMap<String, EntityId> = HashMap::new();
    let mut mission_ids: HashMap<String, EntityId> = HashMap::new();

    // ── Stars
    for s in &doc.stars {
        let eid = EntityId::now_v7();
        let mut props = vec![(PROP_NAME, Value::String(s.name.clone()))];
        if let Some(v) = s.ra            { props.push((PROP_STAR_RA, Value::F64(v))); }
        if let Some(v) = s.dec           { props.push((PROP_STAR_DEC, Value::F64(v))); }
        if let Some(v) = s.distance_pc   { props.push((PROP_STAR_DIST_PC, Value::F64(v))); }
        if let Some(v) = s.teff_k        { props.push((PROP_STAR_TEFF, Value::F64(v))); }
        if let Some(v) = s.mass_msun     { props.push((PROP_STAR_MASS_MSUN, Value::F64(v))); }
        if let Some(v) = s.radius_rsun   { props.push((PROP_STAR_RADIUS_RSUN, Value::F64(v))); }
        if let Some(v) = s.age_gyr       { props.push((PROP_STAR_AGE_GYR, Value::F64(v))); }
        commit_entity(engine, eid, T_STAR, props);
        star_ids.insert(s.name.clone(), eid);
    }

    // ── Methods
    for m in &doc.methods {
        let eid = EntityId::now_v7();
        commit_entity(engine, eid, T_METHOD, vec![
            (PROP_NAME, Value::String(m.name.clone())),
        ]);
        method_ids.insert(m.name.clone(), eid);
    }

    // ── Missions
    for m in &doc.missions {
        let eid = EntityId::now_v7();
        commit_entity(engine, eid, T_MISSION, vec![
            (PROP_NAME, Value::String(m.short.clone())),
            (PROP_MISSION_FULL_NAME, Value::String(m.full.clone())),
        ]);
        mission_ids.insert(m.short.clone(), eid);
    }

    // ── Planets
    for p in &doc.planets {
        let eid = EntityId::now_v7();
        let mut props = vec![
            (PROP_NAME, Value::String(p.name.clone())),
            (PROP_PL_HOST_NAME, Value::String(p.host.clone())),
            (PROP_PL_HABITABLE, Value::String(
                if p.habitable { "yes".into() } else { "no".into() })),
        ];
        if let Some(v) = p.radius_re      { props.push((PROP_PL_RADIUS_RE, Value::F64(v))); }
        if let Some(v) = p.mass_me        { props.push((PROP_PL_MASS_ME, Value::F64(v))); }
        if let Some(v) = p.orbital_period { props.push((PROP_PL_ORBITAL_PERIOD, Value::F64(v))); }
        if let Some(v) = p.semi_major_au  { props.push((PROP_PL_SEMI_MAJOR_AU, Value::F64(v))); }
        if let Some(v) = p.eq_temp_k      { props.push((PROP_PL_EQ_TEMP_K, Value::F64(v))); }
        commit_entity(engine, eid, T_PLANET, props);
        planet_ids.insert(p.name.clone(), eid);
    }

    // ── Discovery hyperedges (arity 4: star, planet, method, mission)
    for d in &doc.discoveries {
        let star = star_ids.get(&d.star).expect("discovery star id");
        let planet = planet_ids.get(&d.planet).expect("discovery planet id");
        let method = method_ids.get(&d.method).expect("discovery method id");
        let mission = mission_ids.get(&d.mission).expect("discovery mission id");

        let roles = vec![
            (RoleId::new(ROLE_STAR), *star),
            (RoleId::new(ROLE_PLANET), *planet),
            (RoleId::new(ROLE_METHOD), *method),
            (RoleId::new(ROLE_MISSION), *mission),
        ];
        let mut props = vec![];
        if let Some(y) = d.year {
            props.push((PROP_DISC_YEAR, Value::I64(y)));
        }
        if !d.facility.is_empty() {
            props.push((PROP_DISC_FACILITY, Value::String(d.facility.clone())));
        }
        if !d.telescope.is_empty() {
            props.push((PROP_DISC_TELESCOPE, Value::String(d.telescope.clone())));
        }
        commit_hyperedge(engine, T_DISCOVERY, roles, props);
    }

    // ── System hyperedges (arity N+1: star, planet_1, …, planet_N)
    for s in &doc.systems {
        let star = star_ids.get(&s.host).expect("system star id");
        let mut roles = vec![(RoleId::new(ROLE_STAR), *star)];
        for pname in &s.planets {
            let pid = planet_ids.get(pname).expect("system planet id");
            roles.push((RoleId::new(ROLE_PLANET), *pid));
        }
        let props = vec![
            (PROP_SYSTEM_HOST, Value::String(s.host.clone())),
            (PROP_PLANET_COUNT, Value::I64(s.n as i64)),
        ];
        commit_hyperedge(engine, T_SYSTEM, roles, props);
    }

    // ── Habitable-zone hyperedges (one per system that has HZ planets)
    for hz in &doc.hz_groups {
        let star = star_ids.get(&hz.host).expect("hz star id");
        let mut roles = vec![(RoleId::new(ROLE_STAR), *star)];
        for pname in &hz.planets {
            let pid = planet_ids.get(pname).expect("hz planet id");
            roles.push((RoleId::new(ROLE_HZ_PLANET), *pid));
        }
        let props = vec![
            (PROP_SYSTEM_HOST, Value::String(hz.host.clone())),
            (PROP_PLANET_COUNT, Value::I64(hz.planets.len() as i64)),
        ];
        commit_hyperedge(engine, T_HABITABLE_ZONE, roles, props);
    }

    eprintln!(
        "seeded {} stars, {} planets, {} methods, {} missions, {} discoveries, {} systems, {} HZ groups",
        doc.stars.len(),
        doc.planets.len(),
        doc.methods.len(),
        doc.missions.len(),
        doc.discoveries.len(),
        doc.systems.len(),
        doc.hz_groups.len(),
    );
    eprintln!(
        "max system arity: {} (largest hyperedge in the demo)",
        doc.systems.iter().map(|s| s.n).max().unwrap_or(0) + 1,
    );
}

// ─── Engine helpers ───────────────────────────────────────────────────

fn count_entities_of_type(engine: &Engine, type_id: u32) -> usize {
    let mut n = 0_usize;
    for r in engine.snapshot_iter_streaming(TxId::ACTIVE).flatten() {
        if let Record::Entity(e) = r
            && e.type_id == TypeId::new(type_id)
        {
            n += 1;
        }
    }
    n
}

fn commit_entity(
    engine: &mut Engine,
    eid: EntityId,
    type_id: u32,
    properties: Vec<(u32, Value)>,
) {
    let mut txn = engine.begin_write();
    let tx_id = txn.tx_id();
    txn.put_entity(EntityRecord {
        entity_id: eid,
        type_id: TypeId::new(type_id),
        tx_id_assert: tx_id,
        tx_id_supersede: TxId::ACTIVE,
        properties: properties
            .into_iter()
            .map(|(p, v)| (PropertyId::new(p), v))
            .collect(),
    });
    txn.commit().expect("commit entity");
}

fn commit_hyperedge(
    engine: &mut Engine,
    type_id: u32,
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
        properties: properties
            .into_iter()
            .map(|(p, v)| (PropertyId::new(p), v))
            .collect(),
    });
    txn.commit().expect("commit hyperedge");
}

// ─── Static file server (cloned from v22_explorer — same shape) ──────

fn serve_static(stream: TcpStream, root: &Path) -> std::io::Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut header = String::new();
    loop {
        header.clear();
        let n = reader.read_line(&mut header)?;
        if n <= 2 {
            break;
        }
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string();
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
        None => {
            write_response(&mut writer, 404, "text/plain", b"not found")?;
        }
    }
    Ok(())
}

fn resolve_path(root: &Path, req_path: &str) -> Option<PathBuf> {
    if req_path.contains("..") {
        return None;
    }
    let trimmed = req_path.trim_start_matches('/');
    let candidate = if trimmed.is_empty() {
        root.join("index.html")
    } else {
        root.join(trimmed)
    };
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

fn write_response<W: Write>(
    w: &mut W,
    status: u16,
    ctype: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Status",
    };
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
