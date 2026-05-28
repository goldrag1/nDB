//! chemistry_ndb — the fourth nDB science demo.
//!
//! Seeds ~43 curated chemical reactions across organic synthesis,
//! inorganic / industrial chemistry, and two whole metabolic pathways
//! (glycolysis 10-step, Krebs cycle 8-step). Each reaction is one
//! arity-N hyperedge whose role-fillers are the reactant + product
//! compound entities (with stoichiometry encoded as repeated role
//! slots — six CO₂ molecules = six ROLE_REACTANT entries pointing
//! at the same CO₂ entity).
//!
//! - `127.0.0.1:8747` — ndb-server for /iter, /commit, /subscribe.
//!   Critically, /subscribe is the wire-level primitive the SPA uses
//!   for its "living-data" reactive demo: edit a reaction condition
//!   on the LEFT pane, the RIGHT pane (a passive /subscribe client)
//!   sees the new value arrive from the engine.
//! - `127.0.0.1:9879` — static file server for docs/chemistry/
//!
//! Mirror of seismic / exoplanet / alphafold explorers.
//!
//! Run:
//! ```sh
//! cargo run -p ndb-renderer --example chemistry_explorer
//! ```
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

// ─── Schema (kept in lockstep with docs/chemistry/index.html) ───────
const T_COMPOUND: u32 = 1;
const T_CATALYST: u32 = 2;
const T_SOLVENT:  u32 = 3;
const T_PATHWAY:  u32 = 4;   // entity; carries an ordered list of reaction names

const T_REACTION: u32 = 100; // arity 2..12 — repeated role slots encode stoichiometry

const ROLE_REACTANT: u32 = 10;
const ROLE_PRODUCT:  u32 = 11;
const ROLE_CATALYST: u32 = 12;
const ROLE_SOLVENT:  u32 = 13;

const PROP_NAME:               u32 = 30;
const PROP_SMILES:             u32 = 31;
const PROP_FORMULA:            u32 = 32;
const PROP_INCHIKEY:           u32 = 33;
const PROP_COMPOUND_KIND:      u32 = 34;
const PROP_EC_NUMBER:          u32 = 35;
const PROP_PATHWAY_SHAPE:      u32 = 36;
const PROP_PATHWAY_REACTIONS_JSON: u32 = 37;

// Reaction edge properties
const PROP_REACTION_NAME:      u32 = 48;
const PROP_TEMPERATURE_K:      u32 = 40;
const PROP_PRESSURE_ATM:       u32 = 41;
const PROP_PH:                 u32 = 42;
const PROP_GIBBS_KJMOL:        u32 = 43;
const PROP_EQUILIBRIUM_K:      u32 = 44;
const PROP_YIELD_PCT:          u32 = 45;
const PROP_CITATION:           u32 = 46;
const PROP_FAMILY:             u32 = 47;
const PROP_PATHWAY_NAME:       u32 = 49;
const PROP_PATHWAY_ORDER:      u32 = 50;
const PROP_NOTE:               u32 = 51;

const API_PORT:    u16 = 8747;
const STATIC_PORT: u16 = 9879;

// Seed JSON baked at build time. ~43 KB.
const SEED_JSON: &str = include_str!("seed.json");

// ─── Seed JSON shape ────────────────────────────────────────────────

#[derive(Deserialize)]
struct SeedDoc {
    compounds: Vec<CompoundRow>,
    catalysts: Vec<CompoundRow>,
    solvents:  Vec<CompoundRow>,
    reactions: Vec<ReactionRow>,
    pathways:  Vec<PathwayRow>,
}

#[derive(Deserialize)]
struct CompoundRow {
    name:     String,
    smiles:   Option<String>,
    formula:  Option<String>,
    inchikey: Option<String>,
    kind:     Option<String>,
    note:     Option<String>,
    ec:       Option<String>,
}

#[derive(Deserialize)]
struct ReactionRow {
    name:          String,
    reactants:     Vec<(f64, String)>,
    products:      Vec<(f64, String)>,
    catalyst:      Option<String>,
    solvent:       Option<String>,
    conditions:    serde_json::Value,
    #[serde(default)]
    gibbs_kjmol:   Option<f64>,
    #[serde(default, rename = "equilibrium_K")]
    equilibrium_k: Option<f64>,
    #[serde(default)]
    yield_pct:     Option<f64>,
    #[serde(default)]
    citation:      Option<String>,
    #[serde(default)]
    family:        Option<String>,
    #[serde(default)]
    pathway:       Option<String>,
    #[serde(default)]
    pathway_order: Option<i64>,
}

#[derive(Deserialize)]
struct PathwayRow {
    name:      String,
    #[allow(dead_code)]  // surfaced to seed.json for the SPA but not used by the seeder
    n_steps:   i64,
    reactions: Vec<String>,
    shape:     String,
}

// ─── Main ──────────────────────────────────────────────────────────

fn main() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().and_then(Path::parent).expect("workspace root");
    let db_dir_owned = workspace_root.join(".demo-data/chemistry-ndb");
    let db_dir = db_dir_owned.as_path();
    if !db_dir.exists() {
        std::fs::create_dir_all(db_dir).expect("mkdir db");
    }
    let mut engine = if Engine::open(db_dir).is_ok() {
        Engine::open(db_dir).expect("open engine")
    } else {
        Engine::create(db_dir).expect("create engine")
    };

    let existing = count_entities_of_type(&engine, T_COMPOUND);
    if existing == 0 {
        eprintln!("first run — seeding chemistry_ndb");
        seed(&mut engine);
        engine.flush().expect("flush");
    } else {
        eprintln!(
            "reusing existing nDB at {} ({} compound entities present)",
            db_dir.display(), existing,
        );
    }
    drop(engine);

    // Public demo. Reads only via the wire — the SPA's /subscribe +
    // /commit dance for the living-data demo still works because it
    // goes through the CLI/internal path during seeding, not the
    // visitor's browser. If a visitor wants to mutate, they'll get
    // 403 — that's the point.
    let server = Arc::new(
        Server::open(db_dir).expect("server open")
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

    let static_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().and_then(Path::parent).expect("workspace root")
        .join("docs/chemistry");
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
                if let Err(e) = serve_static(stream, &root) { eprintln!("static connection: {e}"); }
            });
        }
    });

    loop { std::thread::park(); }
}

// ─── Seed ──────────────────────────────────────────────────────────

fn seed(engine: &mut Engine) {
    let doc: SeedDoc = serde_json::from_str(SEED_JSON).expect("parse seed.json");

    let mut compound_ids: HashMap<String, EntityId> = HashMap::with_capacity(doc.compounds.len());
    let mut catalyst_ids: HashMap<String, EntityId> = HashMap::with_capacity(doc.catalysts.len());
    let mut solvent_ids:  HashMap<String, EntityId> = HashMap::with_capacity(doc.solvents.len());

    for c in &doc.compounds {
        let eid = EntityId::now_v7();
        commit_entity(engine, eid, T_COMPOUND, compound_props(c));
        compound_ids.insert(c.name.clone(), eid);
    }
    for c in &doc.catalysts {
        let eid = EntityId::now_v7();
        commit_entity(engine, eid, T_CATALYST, compound_props(c));
        catalyst_ids.insert(c.name.clone(), eid);
    }
    for c in &doc.solvents {
        let eid = EntityId::now_v7();
        commit_entity(engine, eid, T_SOLVENT, compound_props(c));
        solvent_ids.insert(c.name.clone(), eid);
    }

    // Reactions — each as one hyperedge. Stoichiometry encoded by repeating
    // a role-filler N times. Decimals (e.g. 0.5 O2) round to at least 1 slot.
    let mut reactions_committed = 0;
    let mut max_arity = 0;
    for rx in &doc.reactions {
        let mut roles: Vec<(RoleId, EntityId)> = Vec::new();
        for (stoich, name) in &rx.reactants {
            let Some(eid) = compound_ids.get(name) else {
                eprintln!("  warn: reactant {name:?} not in compounds");
                continue;
            };
            let n = (*stoich).max(1.0).round() as usize;
            for _ in 0..n { roles.push((RoleId::new(ROLE_REACTANT), *eid)); }
        }
        for (stoich, name) in &rx.products {
            let Some(eid) = compound_ids.get(name) else {
                eprintln!("  warn: product {name:?} not in compounds");
                continue;
            };
            let n = (*stoich).max(1.0).round() as usize;
            for _ in 0..n { roles.push((RoleId::new(ROLE_PRODUCT), *eid)); }
        }
        if let Some(cat) = &rx.catalyst {
            if let Some(eid) = catalyst_ids.get(cat) {
                roles.push((RoleId::new(ROLE_CATALYST), *eid));
            }
        }
        if let Some(sol) = &rx.solvent {
            if let Some(eid) = solvent_ids.get(sol) {
                roles.push((RoleId::new(ROLE_SOLVENT), *eid));
            }
        }

        let mut props: Vec<(u32, Value)> = vec![
            (PROP_REACTION_NAME, Value::String(rx.name.clone())),
            (PROP_NAME,          Value::String(rx.name.clone())),
        ];
        // Pull selected conditions onto edge properties so the SPA can
        // mutate them without having to parse a JSON blob.
        if let Some(t) = rx.conditions.get("temperature_K").and_then(|v| v.as_f64()) {
            props.push((PROP_TEMPERATURE_K, Value::F64(t)));
        }
        if let Some(p) = rx.conditions.get("pressure_atm").and_then(|v| v.as_f64()) {
            props.push((PROP_PRESSURE_ATM, Value::F64(p)));
        }
        if let Some(ph) = rx.conditions.get("ph").and_then(|v| v.as_f64()) {
            props.push((PROP_PH, Value::F64(ph)));
        }
        if let Some(v) = rx.gibbs_kjmol   { props.push((PROP_GIBBS_KJMOL,   Value::F64(v))); }
        if let Some(v) = rx.equilibrium_k { props.push((PROP_EQUILIBRIUM_K, Value::F64(v))); }
        if let Some(v) = rx.yield_pct     { props.push((PROP_YIELD_PCT,     Value::F64(v))); }
        if let Some(v) = &rx.citation     { props.push((PROP_CITATION,      Value::String(v.clone()))); }
        if let Some(v) = &rx.family       { props.push((PROP_FAMILY,        Value::String(v.clone()))); }
        if let Some(v) = &rx.pathway      { props.push((PROP_PATHWAY_NAME,  Value::String(v.clone()))); }
        if let Some(v) = rx.pathway_order { props.push((PROP_PATHWAY_ORDER, Value::I64(v))); }

        if roles.len() > max_arity { max_arity = roles.len(); }
        commit_hyperedge(engine, T_REACTION, roles, props);
        reactions_committed += 1;
    }

    // Pathways — entity with PROP_PATHWAY_REACTIONS_JSON listing the
    // ordered reaction names. (Pathway-as-entity-not-hyperedge: nDB roles
    // require EntityId fillers and the natural members here are reaction
    // hyperedges, not entities. The JSON-encoded ordered list is the
    // pragmatic choice — see docs-chemistry_ndb.html for the discussion.)
    for p in &doc.pathways {
        let eid = EntityId::now_v7();
        let props = vec![
            (PROP_NAME,                       Value::String(p.name.clone())),
            (PROP_PATHWAY_SHAPE,              Value::String(p.shape.clone())),
            (PROP_PATHWAY_REACTIONS_JSON,
                Value::String(serde_json::to_string(&p.reactions).unwrap_or_else(|_| "[]".into()))),
        ];
        commit_entity(engine, eid, T_PATHWAY, props);
    }

    eprintln!(
        "seeded {} compounds, {} catalysts, {} solvents, {} reactions, {} pathways",
        doc.compounds.len(),
        doc.catalysts.len(),
        doc.solvents.len(),
        reactions_committed,
        doc.pathways.len(),
    );
    eprintln!("max reaction arity: {max_arity}");
}

fn compound_props(c: &CompoundRow) -> Vec<(u32, Value)> {
    let mut props = vec![(PROP_NAME, Value::String(c.name.clone()))];
    if let Some(v) = &c.smiles   { props.push((PROP_SMILES,   Value::String(v.clone()))); }
    if let Some(v) = &c.formula  { props.push((PROP_FORMULA,  Value::String(v.clone()))); }
    if let Some(v) = &c.inchikey { props.push((PROP_INCHIKEY, Value::String(v.clone()))); }
    if let Some(v) = &c.kind     { props.push((PROP_COMPOUND_KIND, Value::String(v.clone()))); }
    if let Some(v) = &c.ec       { props.push((PROP_EC_NUMBER, Value::String(v.clone()))); }
    if let Some(v) = &c.note     { props.push((PROP_NOTE,     Value::String(v.clone()))); }
    props
}

// ─── Engine helpers (copy from seismic_explorer / exoplanet_explorer) ─

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
        properties: properties.into_iter().map(|(p, v)| (PropertyId::new(p), v)).collect(),
    });
    txn.commit().expect("commit hyperedge");
}

// ─── Static file server ────────────────────────────────────────────

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
