//! v2.2 preview — interactive 3D hypergraph explorer.
#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
)]
//!
//! Seeds a richer biology dataset (proteins + genes + pathways +
//! papers + authors with multi-arity hyperedges between them), then
//! stands up two HTTP servers on localhost:
//!
//! - `127.0.0.1:8742` — `ndb-server` (with CORS) speaking the v2.1
//!   wire protocol. The SPA fetches `/iter` and posts to `/commit`.
//! - `127.0.0.1:9876` — a tiny static file server that serves
//!   `docs/explorer/index.html`.
//!
//! Run from the repo root:
//!
//! ```sh
//! cargo run -p ndb-renderer --example v22_explorer
//! ```
//!
//! Then open <http://127.0.0.1:9876/> in any browser. Drag to rotate,
//! scroll to zoom, click a node to inspect; use the sidebar to add or
//! delete records — the underlying nDB updates live.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ndb_engine::Engine;
use ndb_engine::id::{EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId};
use ndb_engine::record::{EntityRecord, HyperEdgeRecord};
use ndb_engine::value::Value;
use ndb_server::Server;

// Reserved demo IDs — kept in lockstep with the TYPES table in
// docs/explorer/index.html. Changing one without the other will
// detach the SPA's type-name labels from the engine's actual data.
const T_PROTEIN: u32 = 1;
const T_GENE: u32 = 2;
const T_PATHWAY: u32 = 3;
const T_PAPER: u32 = 4;
const T_AUTHOR: u32 = 5;
const T_COMPLEX: u32 = 100;
const T_ENCODES: u32 = 101;
const T_CITES: u32 = 102;
const T_AUTHORED: u32 = 103;

const ROLE_MEMBER: u32 = 10;
const ROLE_GENE: u32 = 11;
const ROLE_PROTEIN: u32 = 12;
const ROLE_PAPER: u32 = 13;
const ROLE_AUTHOR: u32 = 14;
const ROLE_ENTITY: u32 = 15;
const ROLE_PATHWAY: u32 = 16;

const PROP_NAME: u32 = 30;
const PROP_FUNCTION: u32 = 31;
const PROP_YEAR: u32 = 32;
const PROP_TITLE: u32 = 33;
const PROP_PATHWAY_NAME: u32 = 34;

const DB_PATH: &str = "/tmp/v22-explorer-ndb";
const API_PORT: u16 = 8742;
const STATIC_PORT: u16 = 9876;

fn main() {
    // ─── Fresh database ────────────────────────────────────────────
    let db_dir = Path::new(DB_PATH);
    if db_dir.exists() {
        std::fs::remove_dir_all(db_dir).expect("clean prior db");
    }
    std::fs::create_dir_all(db_dir).expect("mkdir db");
    let mut engine = Engine::create(db_dir).expect("create engine");
    seed(&mut engine);
    engine.flush().expect("flush");
    drop(engine);

    // ─── ndb-server with CORS ─────────────────────────────────────
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

    // ─── Static file server ───────────────────────────────────────
    // Serves docs/explorer/ over plain HTTP/1.1. ~50 LOC; no dep.
    let static_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("docs/explorer");
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

    // Block forever (until Ctrl-C). Both server threads keep running.
    loop {
        std::thread::park();
    }
}

fn seed(engine: &mut Engine) {
    // Proteins (15) — name + function + year_discovered.
    let proteins: Vec<(&str, &str, i64)> = vec![
        ("P53", "tumor suppressor", 1979),
        ("MDM2", "ubiquitin ligase", 1991),
        ("ATM", "kinase, DNA damage", 1995),
        ("CHK2", "checkpoint kinase", 1998),
        ("BRCA1", "DNA repair", 1994),
        ("BRCA2", "DNA repair", 1995),
        ("AKT1", "kinase, survival", 1987),
        ("MTOR", "kinase, growth", 1994),
        ("ULK1", "kinase, autophagy", 1998),
        ("BECN1", "autophagy regulator", 1998),
        ("LC3", "autophagy", 2000),
        ("ATG7", "E1-like enzyme", 1999),
        ("PI3K", "kinase, signaling", 1988),
        ("PTEN", "phosphatase", 1997),
        ("KRAS", "GTPase, signaling", 1983),
    ];
    let mut p_ids: Vec<(EntityId, &str)> = Vec::new();
    for (name, func, year) in &proteins {
        let eid = EntityId::now_v7();
        commit_entity(
            engine,
            eid,
            T_PROTEIN,
            vec![
                (PROP_NAME, Value::String((*name).into())),
                (PROP_FUNCTION, Value::String((*func).into())),
                (PROP_YEAR, Value::I64(*year)),
            ],
        );
        p_ids.push((eid, name));
    }

    // Genes (5) — one per "famous" protein.
    let genes: Vec<(&str, &str)> = vec![
        ("TP53", "P53"),
        ("MDM2_g", "MDM2"),
        ("ATM_g", "ATM"),
        ("BRCA1_g", "BRCA1"),
        ("KRAS_g", "KRAS"),
    ];
    let mut g_ids: Vec<(EntityId, &str, &str)> = Vec::new();
    for (gname, encodes_protein) in &genes {
        let eid = EntityId::now_v7();
        commit_entity(
            engine,
            eid,
            T_GENE,
            vec![(PROP_NAME, Value::String((*gname).into()))],
        );
        g_ids.push((eid, gname, encodes_protein));
    }

    // Pathways (4).
    let pathways = vec!["DNA repair", "Autophagy", "mTOR signaling", "Cell cycle"];
    let mut path_ids: Vec<(EntityId, &str)> = Vec::new();
    for p in &pathways {
        let eid = EntityId::now_v7();
        commit_entity(
            engine,
            eid,
            T_PATHWAY,
            vec![
                (PROP_NAME, Value::String((*p).into())),
                (PROP_PATHWAY_NAME, Value::String((*p).into())),
            ],
        );
        path_ids.push((eid, p));
    }

    // Authors (6).
    let authors = vec!["Vogelstein", "Hartwell", "Nurse", "Hunt", "Klionsky", "Levine"];
    let mut a_ids: Vec<(EntityId, &str)> = Vec::new();
    for a in &authors {
        let eid = EntityId::now_v7();
        commit_entity(
            engine,
            eid,
            T_AUTHOR,
            vec![(PROP_NAME, Value::String((*a).into()))],
        );
        a_ids.push((eid, a));
    }

    // Papers (4) — each linking N authors.
    let papers: Vec<(&str, Vec<&str>)> = vec![
        ("p53 surveillance", vec!["Vogelstein"]),
        ("autophagy in cancer", vec!["Klionsky", "Levine"]),
        ("cell-cycle Nobel", vec!["Hartwell", "Nurse", "Hunt"]),
        ("BRCA1 DNA repair", vec!["Vogelstein"]),
    ];
    let mut paper_ids: Vec<(EntityId, &str, Vec<&str>)> = Vec::new();
    for (title, authors_of) in &papers {
        let eid = EntityId::now_v7();
        commit_entity(
            engine,
            eid,
            T_PAPER,
            vec![
                (PROP_TITLE, Value::String((*title).into())),
                (PROP_NAME, Value::String((*title).into())),
            ],
        );
        paper_ids.push((eid, title, authors_of.clone()));
    }

    // ─── Hyperedges ───────────────────────────────────────────────
    let p_by_name = |n: &str| -> EntityId {
        p_ids.iter().find(|(_, name)| *name == n).map_or_else(
            || panic!("missing protein {n}"),
            |(eid, _)| *eid,
        )
    };
    let a_by_name = |n: &str| -> EntityId {
        a_ids.iter().find(|(_, name)| *name == n).map_or_else(
            || panic!("missing author {n}"),
            |(eid, _)| *eid,
        )
    };

    // 6 protein complexes (arity 2-4).
    let complexes: Vec<(&str, &str, Vec<&str>)> = vec![
        ("p53 surveillance", "DNA repair", vec!["P53", "MDM2", "ATM", "CHK2"]),
        ("BRCA repair", "DNA repair", vec!["BRCA1", "BRCA2", "ATM"]),
        ("mTOR growth", "mTOR signaling", vec!["AKT1", "MTOR", "PI3K", "PTEN"]),
        ("autophagy init", "Autophagy", vec!["ULK1", "BECN1", "ATG7"]),
        ("autophagy elongation", "Autophagy", vec!["LC3", "ATG7", "BECN1"]),
        ("Ras signaling", "mTOR signaling", vec!["KRAS", "PI3K", "AKT1"]),
    ];
    for (cname, pathway, members) in &complexes {
        let path_eid = path_ids
            .iter()
            .find(|(_, p)| p == pathway)
            .map(|(e, _)| *e)
            .expect("pathway");
        let mut roles: Vec<(RoleId, EntityId)> = members
            .iter()
            .map(|n| (RoleId::new(ROLE_MEMBER), p_by_name(n)))
            .collect();
        roles.push((RoleId::new(ROLE_PATHWAY), path_eid));
        commit_hyperedge(
            engine,
            T_COMPLEX,
            roles,
            vec![(PROP_NAME, Value::String((*cname).into()))],
        );
    }

    // 5 gene→protein "encodes" hyperedges.
    for (g_eid, _, encodes_protein) in &g_ids {
        commit_hyperedge(
            engine,
            T_ENCODES,
            vec![
                (RoleId::new(ROLE_GENE), *g_eid),
                (RoleId::new(ROLE_PROTEIN), p_by_name(encodes_protein)),
            ],
            vec![],
        );
    }

    // 4 paper→author "authored" hyperedges (arity 2-4).
    for (paper_eid, title, authors_of) in &paper_ids {
        let mut roles: Vec<(RoleId, EntityId)> =
            vec![(RoleId::new(ROLE_PAPER), *paper_eid)];
        for a in authors_of {
            roles.push((RoleId::new(ROLE_AUTHOR), a_by_name(a)));
        }
        commit_hyperedge(
            engine,
            T_AUTHORED,
            roles,
            vec![(PROP_TITLE, Value::String((*title).into()))],
        );
    }

    // A few "cites" hyperedges linking papers to proteins they
    // mention. Demonstrates cross-type N-ary edges.
    let cites: Vec<(&str, Vec<&str>)> = vec![
        ("p53 surveillance", vec!["P53", "MDM2", "ATM"]),
        ("autophagy in cancer", vec!["LC3", "BECN1", "ATG7", "ULK1"]),
        ("BRCA1 DNA repair", vec!["BRCA1", "BRCA2", "ATM"]),
    ];
    for (title, proteins_mentioned) in &cites {
        let paper_eid = paper_ids
            .iter()
            .find(|(_, t, _)| t == title)
            .map(|(e, _, _)| *e)
            .expect("paper");
        let mut roles: Vec<(RoleId, EntityId)> =
            vec![(RoleId::new(ROLE_PAPER), paper_eid)];
        for p in proteins_mentioned {
            roles.push((RoleId::new(ROLE_ENTITY), p_by_name(p)));
        }
        commit_hyperedge(engine, T_CITES, roles, vec![]);
    }
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

// ─── Tiny static file server ──────────────────────────────────────

#[allow(clippy::needless_pass_by_value)] // stream is moved into the function and consumed
fn serve_static(stream: TcpStream, root: &Path) -> std::io::Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Discard headers until blank line.
    let mut header = String::new();
    loop {
        header.clear();
        let n = reader.read_line(&mut header)?;
        if n <= 2 {
            break;
        }
    }
    // GET /path HTTP/1.1
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string();
    let mut writer = &stream;
    let target = resolve_path(root, &path);
    match target {
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
    // Reject any traversal sequences.
    if req_path.contains("..") {
        return None;
    }
    let trimmed = req_path.trim_start_matches('/');
    let candidate = if trimmed.is_empty() {
        root.join("index.html")
    } else {
        root.join(trimmed)
    };
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

fn content_type(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn write_response<W: Write>(w: &mut W, code: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = if code == 404 { "Not Found" } else { "OK" };
    write!(
        w,
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {ctype}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len(),
    )?;
    w.write_all(body)?;
    w.flush()
}

