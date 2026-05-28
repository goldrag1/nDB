//! biodiv_ndb — the fifth nDB science demo.
//!
//! Ecological interactions as N-ary hyperedges. The angle each previous
//! demo missed: **temporal qualifiers** (each interaction carries a
//! month-range when it's valid: e.g. yucca/yucca-moth = May–July only).
//!
//! Five hyperedge types:
//!   - pollination : arity 3 — (plant, pollinator, region) + obligate? + season window
//!   - mutualism   : arity 3 — (species_a, species_b, region) + subtype
//!   - parasitism  : arity 3 — (host, parasite, region) + transmission mode
//!   - predation   : arity 3 — (predator, prey, region) + season window
//!   - food_web    : arity (1 + N) — (ecosystem, member_1, …, member_N) — up to 18
//!
//! The food-web record is the big-arity story: one record per ecosystem
//! holding 10–17 member species. The Yellowstone / kelp-forest /
//! Serengeti / coral-reef / arctic / Amazon food webs each fit into a
//! single nDB row.
//!
//! - `127.0.0.1:8748` — ndb-server for /iter, /commit, /subscribe
//! - `127.0.0.1:9881` — static file server for docs/biodiv/
//!
//! Run:
//! ```sh
//! cargo run -p ndb-renderer --example biodiv_explorer
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
use ndb_engine::record::{EntityRecord, HyperEdgeRecord, PropertyKeyRecord, Record, RoleNameRecord, TypeNameRecord};
use ndb_engine::value::Value;
use ndb_server::Server;
use serde::Deserialize;

// ─── Schema (kept in lockstep with docs/biodiv/index.html) ─────────────
const T_SPECIES:     u32 = 1;
const T_REGION:      u32 = 2;

const T_POLLINATION: u32 = 100;
const T_MUTUALISM:   u32 = 101;
const T_PARASITISM:  u32 = 102;
const T_PREDATION:   u32 = 103;
const T_FOOD_WEB:    u32 = 104;

const ROLE_PLANT:        u32 = 10;
const ROLE_POLLINATOR:   u32 = 11;
const ROLE_MUTUALIST_A:  u32 = 12;
const ROLE_MUTUALIST_B:  u32 = 13;
const ROLE_HOST:         u32 = 14;
const ROLE_PARASITE:     u32 = 15;
const ROLE_PREDATOR:     u32 = 16;
const ROLE_PREY:         u32 = 17;
const ROLE_REGION:       u32 = 18;
const ROLE_ECOSYSTEM:    u32 = 19;
const ROLE_MEMBER:       u32 = 20;

const PROP_NAME:                u32 = 30;
const PROP_SCIENTIFIC_NAME:     u32 = 31;
const PROP_COMMON_NAME:         u32 = 32;
const PROP_KINGDOM:             u32 = 33;
const PROP_FAMILY:              u32 = 34;
const PROP_LIFE_FORM:           u32 = 35;
const PROP_PHOTO_URL:           u32 = 36;
const PROP_WIKI_URL:            u32 = 37;
const PROP_REGION_KIND:         u32 = 38;
const PROP_LATITUDE:            u32 = 39;
const PROP_LONGITUDE:           u32 = 40;
const PROP_REGION_KEY:          u32 = 41;

const PROP_SEASON_FROM:         u32 = 50;
const PROP_SEASON_TO:           u32 = 51;
const PROP_OBLIGATE:            u32 = 52;
const PROP_INTERACTION_SUBTYPE: u32 = 53;
const PROP_TRANSMISSION:        u32 = 54;
const PROP_NOTE:                u32 = 55;
const PROP_FOOD_WEB_NAME:       u32 = 56;
const PROP_INTERACTION_KIND:    u32 = 57;
const PROP_TROPHIC_EDGES_JSON:  u32 = 58;  // JSON: [[predator_sci, prey_sci], ...]

const API_PORT:    u16 = 8748;
const STATIC_PORT: u16 = 9881;

// Seed JSON baked at build time (~80 KB).
const SEED_JSON: &str = include_str!("seed.json");

// ─── Seed JSON shape ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct SeedDoc {
    regions:     Vec<RegionRow>,
    species:     Vec<SpeciesRow>,
    pollination: Vec<PollinationRow>,
    mutualism:   Vec<MutualismRow>,
    parasitism:  Vec<ParasitismRow>,
    predation:   Vec<PredationRow>,
    food_webs:   Vec<FoodWebRow>,
}

#[derive(Deserialize)]
struct RegionRow {
    key:  String,
    name: String,
    kind: String,
    lat:  f64,
    lon:  f64,
}

#[derive(Deserialize)]
struct SpeciesRow {
    sci:        String,
    common:     String,
    kingdom:    String,
    family:     String,
    life_form:  String,
    photo_url:  String,
    wiki_url:   String,
}

#[derive(Deserialize)]
struct PollinationRow {
    plant:       String,
    pollinator:  String,
    region:      String,
    season_from: i64,
    season_to:   i64,
    obligate:    bool,
    note:        String,
}

#[derive(Deserialize)]
struct MutualismRow {
    species_a: String,
    species_b: String,
    region:    String,
    subtype:   String,
    obligate:  bool,
    note:      String,
}

#[derive(Deserialize)]
struct ParasitismRow {
    host:         String,
    parasite:     String,
    region:       String,
    transmission: String,
    note:         String,
}

#[derive(Deserialize)]
struct PredationRow {
    predator:    String,
    prey:        String,
    region:      String,
    season_from: i64,
    season_to:   i64,
    note:        String,
}

#[derive(Deserialize)]
struct FoodWebRow {
    name:           String,
    ecosystem:      String,
    members:        Vec<String>,
    #[serde(default)]
    trophic_edges:  Vec<(String, String)>,
    note:           String,
}

// ─── Main ──────────────────────────────────────────────────────────────

fn main() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().and_then(Path::parent).expect("workspace root");
    let db_dir_owned = workspace_root.join(".demo-data/biodiv-ndb");
    let db_dir = db_dir_owned.as_path();
    if !db_dir.exists() {
        std::fs::create_dir_all(db_dir).expect("mkdir db");
    }
    let mut engine = if Engine::open(db_dir).is_ok() {
        Engine::open(db_dir).expect("open engine")
    } else {
        Engine::create(db_dir).expect("create engine")
    };

    let existing = count_entities_of_type(&engine, T_SPECIES);
    if existing == 0 {
        eprintln!("first run — seeding biodiv_ndb");
        seed(&mut engine);
        engine.flush().expect("flush");
    } else {
        eprintln!(
            "reusing existing nDB at {} ({} species entities present)",
            db_dir.display(), existing,
        );
    }
    drop(engine);

    // Public demo — exposed via the knowledge-site proxy. Reads only.
    // Visitors must not be able to mutate the demo data via the wire.
    // In-process seeding above is unaffected.
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
        .join("docs/biodiv");
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

// ─── Seed ──────────────────────────────────────────────────────────────

fn seed(engine: &mut Engine) {
    let doc: SeedDoc = serde_json::from_str(SEED_JSON).expect("parse seed.json");

    // Register name dictionaries so the text-query endpoint can resolve
    // "species", "pollinator", "season_from", etc. → ids without the
    // client having to send raw ids.
    seed_name_dictionaries(engine);

    let mut region_ids:  HashMap<String, EntityId> = HashMap::with_capacity(doc.regions.len());
    let mut species_ids: HashMap<String, EntityId> = HashMap::with_capacity(doc.species.len());

    for r in &doc.regions {
        let eid = EntityId::now_v7();
        let props = vec![
            (PROP_NAME,         Value::String(r.name.clone())),
            (PROP_REGION_KEY,   Value::String(r.key.clone())),
            (PROP_REGION_KIND,  Value::String(r.kind.clone())),
            (PROP_LATITUDE,     Value::F64(r.lat)),
            (PROP_LONGITUDE,    Value::F64(r.lon)),
        ];
        commit_entity(engine, eid, T_REGION, props);
        region_ids.insert(r.key.clone(), eid);
    }
    for s in &doc.species {
        let eid = EntityId::now_v7();
        let mut props = vec![
            (PROP_NAME,             Value::String(s.common.clone())),
            (PROP_SCIENTIFIC_NAME,  Value::String(s.sci.clone())),
            (PROP_COMMON_NAME,      Value::String(s.common.clone())),
            (PROP_KINGDOM,          Value::String(s.kingdom.clone())),
            (PROP_FAMILY,           Value::String(s.family.clone())),
            (PROP_LIFE_FORM,        Value::String(s.life_form.clone())),
        ];
        if !s.photo_url.is_empty() { props.push((PROP_PHOTO_URL, Value::String(s.photo_url.clone()))); }
        if !s.wiki_url.is_empty()  { props.push((PROP_WIKI_URL,  Value::String(s.wiki_url.clone()))); }
        commit_entity(engine, eid, T_SPECIES, props);
        species_ids.insert(s.sci.clone(), eid);
    }

    let mut max_arity = 0_usize;

    // Pollination
    let mut n_poll = 0;
    for p in &doc.pollination {
        let (Some(&plant), Some(&pollinator), Some(&region)) = (
            species_ids.get(&p.plant),
            species_ids.get(&p.pollinator),
            region_ids.get(&p.region),
        ) else {
            eprintln!("  warn: pollination row references unknown taxon/region: {} ↔ {}", p.plant, p.pollinator);
            continue;
        };
        let roles = vec![
            (RoleId::new(ROLE_PLANT),      plant),
            (RoleId::new(ROLE_POLLINATOR), pollinator),
            (RoleId::new(ROLE_REGION),     region),
        ];
        max_arity = max_arity.max(roles.len());
        let props = vec![
            (PROP_INTERACTION_KIND, Value::String("pollination".to_string())),
            (PROP_SEASON_FROM,      Value::I64(p.season_from)),
            (PROP_SEASON_TO,        Value::I64(p.season_to)),
            (PROP_OBLIGATE,         Value::Bool(p.obligate)),
            (PROP_NOTE,             Value::String(p.note.clone())),
        ];
        commit_hyperedge(engine, T_POLLINATION, roles, props);
        n_poll += 1;
    }

    // Mutualism
    let mut n_mut = 0;
    for m in &doc.mutualism {
        let (Some(&a), Some(&b), Some(&region)) = (
            species_ids.get(&m.species_a),
            species_ids.get(&m.species_b),
            region_ids.get(&m.region),
        ) else {
            eprintln!("  warn: mutualism row references unknown taxon/region: {} ↔ {}", m.species_a, m.species_b);
            continue;
        };
        let roles = vec![
            (RoleId::new(ROLE_MUTUALIST_A), a),
            (RoleId::new(ROLE_MUTUALIST_B), b),
            (RoleId::new(ROLE_REGION),      region),
        ];
        max_arity = max_arity.max(roles.len());
        let props = vec![
            (PROP_INTERACTION_KIND,     Value::String("mutualism".to_string())),
            (PROP_INTERACTION_SUBTYPE,  Value::String(m.subtype.clone())),
            (PROP_OBLIGATE,             Value::Bool(m.obligate)),
            (PROP_NOTE,                 Value::String(m.note.clone())),
        ];
        commit_hyperedge(engine, T_MUTUALISM, roles, props);
        n_mut += 1;
    }

    // Parasitism
    let mut n_par = 0;
    for p in &doc.parasitism {
        let (Some(&host), Some(&parasite), Some(&region)) = (
            species_ids.get(&p.host),
            species_ids.get(&p.parasite),
            region_ids.get(&p.region),
        ) else {
            eprintln!("  warn: parasitism row references unknown taxon/region: {} ↔ {}", p.host, p.parasite);
            continue;
        };
        let roles = vec![
            (RoleId::new(ROLE_HOST),     host),
            (RoleId::new(ROLE_PARASITE), parasite),
            (RoleId::new(ROLE_REGION),   region),
        ];
        max_arity = max_arity.max(roles.len());
        let props = vec![
            (PROP_INTERACTION_KIND, Value::String("parasitism".to_string())),
            (PROP_TRANSMISSION,     Value::String(p.transmission.clone())),
            (PROP_NOTE,             Value::String(p.note.clone())),
        ];
        commit_hyperedge(engine, T_PARASITISM, roles, props);
        n_par += 1;
    }

    // Predation
    let mut n_pred = 0;
    for p in &doc.predation {
        let (Some(&predator), Some(&prey), Some(&region)) = (
            species_ids.get(&p.predator),
            species_ids.get(&p.prey),
            region_ids.get(&p.region),
        ) else {
            eprintln!("  warn: predation row references unknown taxon/region: {} ↔ {}", p.predator, p.prey);
            continue;
        };
        let roles = vec![
            (RoleId::new(ROLE_PREDATOR), predator),
            (RoleId::new(ROLE_PREY),     prey),
            (RoleId::new(ROLE_REGION),   region),
        ];
        max_arity = max_arity.max(roles.len());
        let props = vec![
            (PROP_INTERACTION_KIND, Value::String("predation".to_string())),
            (PROP_SEASON_FROM,      Value::I64(p.season_from)),
            (PROP_SEASON_TO,        Value::I64(p.season_to)),
            (PROP_NOTE,             Value::String(p.note.clone())),
        ];
        commit_hyperedge(engine, T_PREDATION, roles, props);
        n_pred += 1;
    }

    // Food webs — one big-arity hyperedge per ecosystem
    let mut n_fw = 0;
    for fw in &doc.food_webs {
        let Some(&ecosystem) = region_ids.get(&fw.ecosystem) else {
            eprintln!("  warn: food_web ecosystem unknown: {}", fw.ecosystem);
            continue;
        };
        let mut roles: Vec<(RoleId, EntityId)> = vec![(RoleId::new(ROLE_ECOSYSTEM), ecosystem)];
        for m in &fw.members {
            if let Some(&eid) = species_ids.get(m) {
                roles.push((RoleId::new(ROLE_MEMBER), eid));
            } else {
                eprintln!("  warn: food_web member unknown: {m}");
            }
        }
        max_arity = max_arity.max(roles.len());
        let trophic_json = serde_json::to_string(&fw.trophic_edges).unwrap_or_else(|_| "[]".into());
        let props = vec![
            (PROP_INTERACTION_KIND,    Value::String("food_web".to_string())),
            (PROP_FOOD_WEB_NAME,       Value::String(fw.name.clone())),
            (PROP_NAME,                Value::String(fw.name.clone())),
            (PROP_NOTE,                Value::String(fw.note.clone())),
            (PROP_TROPHIC_EDGES_JSON,  Value::String(trophic_json)),
        ];
        commit_hyperedge(engine, T_FOOD_WEB, roles, props);
        n_fw += 1;
    }

    eprintln!(
        "seeded {} regions, {} species; interactions: {} pollination, {} mutualism, {} parasitism, {} predation, {} food_webs",
        doc.regions.len(), doc.species.len(), n_poll, n_mut, n_par, n_pred, n_fw,
    );
    eprintln!("max hyperedge arity: {max_arity}");
}

fn seed_name_dictionaries(engine: &mut Engine) {
    let types: &[(u32, &str)] = &[
        (T_SPECIES,     "species"),
        (T_REGION,      "region"),
        (T_POLLINATION, "pollination"),
        (T_MUTUALISM,   "mutualism"),
        (T_PARASITISM,  "parasitism"),
        (T_PREDATION,   "predation"),
        (T_FOOD_WEB,    "food_web"),
    ];
    let roles: &[(u32, &str)] = &[
        (ROLE_PLANT, "plant"), (ROLE_POLLINATOR, "pollinator"),
        (ROLE_MUTUALIST_A, "mutualist_a"), (ROLE_MUTUALIST_B, "mutualist_b"),
        (ROLE_HOST, "host"), (ROLE_PARASITE, "parasite"),
        (ROLE_PREDATOR, "predator"), (ROLE_PREY, "prey"),
        (ROLE_REGION, "region"), (ROLE_ECOSYSTEM, "ecosystem"),
        (ROLE_MEMBER, "member"),
    ];
    let props: &[(u32, &str)] = &[
        (PROP_NAME, "name"), (PROP_SCIENTIFIC_NAME, "scientific_name"),
        (PROP_COMMON_NAME, "common_name"), (PROP_KINGDOM, "kingdom"),
        (PROP_FAMILY, "family"), (PROP_LIFE_FORM, "life_form"),
        (PROP_PHOTO_URL, "photo_url"), (PROP_WIKI_URL, "wiki_url"),
        (PROP_REGION_KIND, "region_kind"), (PROP_LATITUDE, "latitude"),
        (PROP_LONGITUDE, "longitude"), (PROP_REGION_KEY, "region_key"),
        (PROP_SEASON_FROM, "season_from"), (PROP_SEASON_TO, "season_to"),
        (PROP_OBLIGATE, "obligate"), (PROP_INTERACTION_SUBTYPE, "interaction_subtype"),
        (PROP_TRANSMISSION, "transmission"), (PROP_NOTE, "note"),
        (PROP_FOOD_WEB_NAME, "food_web_name"), (PROP_INTERACTION_KIND, "interaction_kind"),
        (PROP_TROPHIC_EDGES_JSON, "trophic_edges_json"),
    ];
    let mut txn = engine.begin_write();
    for (id, n) in types {
        txn.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(*id), name: (*n).into() }));
    }
    for (id, n) in roles {
        txn.put_raw(Record::RoleName(RoleNameRecord { id: RoleId::new(*id), name: (*n).into() }));
    }
    for (id, n) in props {
        txn.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(*id), name: (*n).into() }));
    }
    txn.commit().expect("commit dictionaries");
}

// ─── Engine helpers ────────────────────────────────────────────────────

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

// ─── Static file server ────────────────────────────────────────────────

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
        Some("jpg") | Some("jpeg") => "image/jpeg",
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
