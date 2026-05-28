//! v2.2 preview — interactive 3D hypergraph explorer.
#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
)]
//!
//! Seeds a structural-biology dataset (proteins + their encoding
//! genes + pathways + multi-subunit protein complexes + per-residue
//! entities + structural-motif hyperedges), then stands up two HTTP
//! servers on localhost:
//!
//! - `127.0.0.1:8742` — `ndb-server` (with CORS) speaking the nDB
//!   JSON wire protocol. The SPA fetches `/iter` and posts to `/commit`.
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

mod residues;

// Reserved demo IDs — kept in lockstep with the TYPES table in
// docs/explorer/index.html. Changing one without the other will
// detach the SPA's type-name labels from the engine's actual data.
// Type IDs are non-contiguous so they can grow without renumbering;
// gaps (4, 5, 102, 103, 112) are intentional placeholders.
const T_PROTEIN: u32 = 1;
const T_GENE: u32 = 2;
const T_PATHWAY: u32 = 3;
// Reserved for per-atom entities written by the SPA after a CIF loads.
// nDB doesn't seed atoms — the SPA decomposes the CIF (which lives in
// the protein entity's PROP_CIF_BYTES) into one entity per atom on
// first view, so atom-level queries can be served from indexed nDB
// records instead of re-parsing the CIF every call.
#[allow(dead_code)]
const T_ATOM: u32 = 7;
const T_COMPLEX: u32 = 100;
const T_ENCODES: u32 = 101;
#[allow(dead_code)]
const T_ATOM_OF: u32 = 117; // binary hyperedge: atom → protein

const ROLE_MEMBER: u32 = 10;
const ROLE_GENE: u32 = 11;
const ROLE_PROTEIN: u32 = 12;
const ROLE_PATHWAY: u32 = 16;

const PROP_NAME: u32 = 30;
const PROP_FUNCTION: u32 = 31;
const PROP_YEAR: u32 = 32;
const PROP_PATHWAY_NAME: u32 = 34;
// (Was PROP_CIF_BYTES = 35 — removed. The CIF blob is no longer stored
// in nDB; atom entities are the canonical structural representation.)
// AlphaFold-derived properties (v2.2 §A — confidence overlay).
const PROP_PLDDT_MEAN: u32 = 36;
const PROP_PLDDT_BUCKET: u32 = 37;
const PROP_COMPLEX_CONFIDENCE: u32 = 38;
// Auxiliary metadata shipped from the AF-DB record (v2.2 §B — used when
// the user fetches a new protein live, but seeded too for the 15
// curated entries so the model is symmetric).
const PROP_UNIPROT: u32 = 39;
const PROP_SEQ_LEN: u32 = 40;
const PROP_ORGANISM: u32 = 41;
const PROP_GENE: u32 = 42;

const API_PORT: u16 = 8742;
const STATIC_PORT: u16 = 9876;

fn main() {
    // ─── Persistent database ───────────────────────────────────────
    // Data committed on previous runs stays on disk — including any
    // CIFs cached by "Warm cache", any atom entities the SPA wrote
    // back, and any proteins added via the Fetch form. We only run
    // the curated seed when the engine has zero protein records.
    //
    // Path is project-local (`<workspace>/.demo-data/alphafold-ndb`)
    // so the demo survives /tmp wipes on reboot. CARGO_MANIFEST_DIR
    // is baked in at build time → resolves regardless of cwd.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let db_dir_owned = workspace_root.join(".demo-data/alphafold-ndb");
    let db_dir = db_dir_owned.as_path();
    if !db_dir.exists() {
        std::fs::create_dir_all(db_dir).expect("mkdir db");
    }
    let mut engine = if Engine::open(db_dir).is_ok() {
        Engine::open(db_dir).expect("open engine")
    } else {
        Engine::create(db_dir).expect("create engine")
    };
    let existing_proteins = count_entities_of_type(&engine, T_PROTEIN);
    if existing_proteins == 0 {
        eprintln!("first run — seeding 20 proteins + structural motifs");
        seed(&mut engine);
        engine.flush().expect("flush");
    } else {
        eprintln!(
            "reusing existing nDB at {} ({} proteins already stored; CIFs + atoms preserved)",
            db_dir.display(), existing_proteins
        );
    }
    drop(engine);

    // ─── ndb-server with CORS — public demo, read-only via the wire.
    // The SPA's "live AlphaFold-DB fetch" feature stops working from
    // the browser. The launcher / CLI can still write directly.
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

/// Count entities of a given type currently visible in the engine.
/// Used at boot to decide whether to seed: zero protein entities →
/// fresh DB, run the seed; otherwise reuse the existing state.
fn count_entities_of_type(engine: &Engine, type_id: u32) -> usize {
    let mut n = 0_usize;
    for r in engine.snapshot_iter_streaming(TxId::ACTIVE).flatten() {
        if let ndb_engine::record::Record::Entity(e) = r
            && e.type_id == TypeId::new(type_id)
        {
            n += 1;
        }
    }
    n
}

/// Bucket per AlphaFold-DB published thresholds.
/// <https://alphafold.ebi.ac.uk/faq#faq-5>
fn plddt_bucket(mean: f64) -> &'static str {
    if mean > 90.0 {
        "very_high"
    } else if mean > 70.0 {
        "confident"
    } else if mean > 50.0 {
        "low"
    } else {
        "very_low"
    }
}

/// Curated AlphaFold-DB metadata for the 15 seed proteins.
/// Values fetched + verified from `alphafold.ebi.ac.uk/api/prediction/<acc>`
/// in May 2026 (`model_v6`). [`None`] for the two extra-large proteins (ATM
/// 3056 aa, BRCA2 3418 aa) whose predictions AF-DB has retired — the viz
/// renders them with neutral colouring so the user can see "no AF data".
struct AfRecord {
    name: &'static str,
    func: &'static str,
    year: i64,
    uniprot: &'static str,
    gene: &'static str,
    organism: &'static str,
    seq_len: i64,
    plddt_mean: Option<f64>,
}
const AF_SEED: &[AfRecord] = &[
    AfRecord { name: "P53",   func: "tumor suppressor",   year: 1979, uniprot: "P04637", gene: "TP53",     organism: "Homo sapiens", seq_len: 393,  plddt_mean: Some(75.06) },
    AfRecord { name: "MDM2",  func: "ubiquitin ligase",   year: 1991, uniprot: "Q00987", gene: "MDM2",     organism: "Homo sapiens", seq_len: 491,  plddt_mean: Some(62.59) },
    AfRecord { name: "ATM",   func: "kinase, DNA damage", year: 1995, uniprot: "Q13315", gene: "ATM",      organism: "Homo sapiens", seq_len: 3056, plddt_mean: None         },
    AfRecord { name: "CHK2",  func: "checkpoint kinase",  year: 1998, uniprot: "O96017", gene: "CHEK2",    organism: "Homo sapiens", seq_len: 543,  plddt_mean: Some(76.19) },
    AfRecord { name: "BRCA1", func: "DNA repair",         year: 1994, uniprot: "P38398", gene: "BRCA1",    organism: "Homo sapiens", seq_len: 1863, plddt_mean: Some(41.59) },
    AfRecord { name: "BRCA2", func: "DNA repair",         year: 1995, uniprot: "P51587", gene: "BRCA2",    organism: "Homo sapiens", seq_len: 3418, plddt_mean: None         },
    AfRecord { name: "AKT1",  func: "kinase, survival",   year: 1987, uniprot: "P31749", gene: "AKT1",     organism: "Homo sapiens", seq_len: 480,  plddt_mean: Some(83.06) },
    AfRecord { name: "MTOR",  func: "kinase, growth",     year: 1994, uniprot: "P42345", gene: "MTOR",     organism: "Homo sapiens", seq_len: 2549, plddt_mean: Some(78.00) },
    AfRecord { name: "ULK1",  func: "kinase, autophagy",  year: 1998, uniprot: "O75385", gene: "ULK1",     organism: "Homo sapiens", seq_len: 1050, plddt_mean: Some(59.41) },
    AfRecord { name: "BECN1", func: "autophagy regulator",year: 1998, uniprot: "Q14457", gene: "BECN1",    organism: "Homo sapiens", seq_len: 450,  plddt_mean: Some(76.56) },
    AfRecord { name: "LC3",   func: "autophagy",          year: 2000, uniprot: "Q9GZQ8", gene: "MAP1LC3B", organism: "Homo sapiens", seq_len: 125,  plddt_mean: Some(91.44) },
    AfRecord { name: "ATG7",  func: "E1-like enzyme",     year: 1999, uniprot: "O95352", gene: "ATG7",     organism: "Homo sapiens", seq_len: 703,  plddt_mean: Some(87.62) },
    AfRecord { name: "PI3K",  func: "kinase, signaling",  year: 1988, uniprot: "P42336", gene: "PIK3CA",   organism: "Homo sapiens", seq_len: 1068, plddt_mean: Some(92.38) },
    AfRecord { name: "PTEN",  func: "phosphatase",        year: 1997, uniprot: "P60484", gene: "PTEN",     organism: "Homo sapiens", seq_len: 403,  plddt_mean: Some(83.00) },
    AfRecord { name: "KRAS",  func: "GTPase, signaling",  year: 1983, uniprot: "P01116", gene: "KRAS",     organism: "Homo sapiens", seq_len: 189,  plddt_mean: Some(91.50) },
    // ─── Structural-biology showcase parents for the v2.2 §C residue
    // dataset. These are the five proteins whose residue-level data
    // lives in residues.rs, picked from across the structural-biology
    // canon (serine protease, zinc finger, disulfide bonds, helix
    // sandwich, β-barrel). pLDDT values fetched May 2026.
    AfRecord { name: "Trypsin",   func: "serine protease",            year: 1876, uniprot: "P00760", gene: "PRSS1",  organism: "Bos taurus",            seq_len: 246, plddt_mean: Some(93.12) },
    AfRecord { name: "TFIIIA",    func: "zinc-finger transcription",  year: 1980, uniprot: "P03001", gene: "gtf3a",  organism: "Xenopus laevis",        seq_len: 366, plddt_mean: Some(71.00) },
    AfRecord { name: "Insulin",   func: "hormone, glucose regulation",year: 1921, uniprot: "P01308", gene: "INS",    organism: "Homo sapiens",          seq_len: 110, plddt_mean: Some(52.91) },
    AfRecord { name: "Myoglobin", func: "oxygen storage",             year: 1958, uniprot: "P02185", gene: "MB",     organism: "Physeter macrocephalus",seq_len: 154, plddt_mean: Some(97.50) },
    AfRecord { name: "GFP",       func: "fluorescent reporter",       year: 1962, uniprot: "P42212", gene: "GFP",    organism: "Aequorea victoria",     seq_len: 238, plddt_mean: Some(96.62) },
];

fn seed(engine: &mut Engine) {
    // Proteins (15) — name + function + year_discovered + AF-DB confidence.
    let mut p_ids: Vec<(EntityId, &str, Option<f64>)> = Vec::new();
    for rec in AF_SEED {
        let eid = EntityId::now_v7();
        let mut props: Vec<(u32, Value)> = vec![
            (PROP_NAME, Value::String(rec.name.into())),
            (PROP_FUNCTION, Value::String(rec.func.into())),
            (PROP_YEAR, Value::I64(rec.year)),
            (PROP_UNIPROT, Value::String(rec.uniprot.into())),
            (PROP_GENE, Value::String(rec.gene.into())),
            (PROP_ORGANISM, Value::String(rec.organism.into())),
            (PROP_SEQ_LEN, Value::I64(rec.seq_len)),
        ];
        if let Some(mean) = rec.plddt_mean {
            props.push((PROP_PLDDT_MEAN, Value::F64(mean)));
            props.push((PROP_PLDDT_BUCKET, Value::String(plddt_bucket(mean).into())));
        }
        commit_entity(engine, eid, T_PROTEIN, props);
        p_ids.push((eid, rec.name, rec.plddt_mean));
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

    // ─── Hyperedges ───────────────────────────────────────────────
    let p_by_name = |n: &str| -> EntityId {
        p_ids.iter().find(|(_, name, _)| *name == n).map_or_else(
            || panic!("missing protein {n}"),
            |(eid, _, _)| *eid,
        )
    };
    let p_plddt = |n: &str| -> Option<f64> {
        p_ids
            .iter()
            .find(|(_, name, _)| *name == n)
            .and_then(|(_, _, m)| *m)
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
        // Synthesise a complex-level confidence from member pLDDT means.
        // We don't have AlphaFold-Multimer's ipTM, so use a "mean × 0.8
        // + min × 0.2" proxy — the weakest member drags the score down,
        // which mirrors how confidence actually propagates through a
        // multi-subunit complex prediction. Skip the property entirely
        // when any member lacks an AF-DB record (no fabrication).
        let mut props: Vec<(u32, Value)> = vec![(PROP_NAME, Value::String((*cname).into()))];
        let member_plddts: Option<Vec<f64>> =
            members.iter().map(|n| p_plddt(n)).collect();
        if let Some(plddts) = member_plddts
            && !plddts.is_empty()
        {
            // Members is always small (≤ ~6 in this dataset), well within
            // f64 mantissa precision — but clippy can't see that, so opt
            // in to the cast.
            #[allow(clippy::cast_precision_loss)]
            let n = plddts.len() as f64;
            let mean = plddts.iter().sum::<f64>() / n;
            let min = plddts.iter().copied().fold(f64::INFINITY, f64::min);
            let combined = mean.mul_add(0.8, min * 0.2);
            props.push((PROP_COMPLEX_CONFIDENCE, Value::F64(combined)));
        }
        commit_hyperedge(engine, T_COMPLEX, roles, props);
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

    // ─── Residue-level dataset (v2.2 §C) ─────────────────────────
    // For five well-characterised proteins we additionally commit one
    // entity per residue + one hyperedge per structural motif. The
    // motif hyperedges are intrinsically N-ary (catalytic triad of 3,
    // zinc finger of 4, helix of 16, β-sheet pair of 20) — the case
    // where a binary-edge model would have to reify each motif as a
    // dummy node, and nDB just stores the relationship directly.
    let stats = residues::seed_all(engine, &|name| {
        p_ids
            .iter()
            .find(|(_, n, _)| *n == name)
            .map(|(eid, _, _)| *eid)
    });
    eprintln!(
        "seeded {} residue entities, {} motif hyperedges \
         (triad={}, disulfide={}, zinc_finger={}, alpha_helix={}, beta_sheet_pair={})",
        stats.residues,
        stats.motifs,
        stats.by_type[0], stats.by_type[1], stats.by_type[2],
        stats.by_type[3], stats.by_type[4],
    );
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
        hyperedge_roles: Vec::new(),
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
         Cache-Control: no-store, no-cache, must-revalidate\r\n\
         Connection: close\r\n\r\n",
        body.len(),
    )?;
    w.write_all(body)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndb_engine::id::PropertyId;

    #[test]
    fn plddt_bucket_matches_alphafold_db_thresholds() {
        // AlphaFold-DB FAQ §5 published cutoffs (May 2026):
        //   pLDDT > 90  → very_high
        //   pLDDT > 70  → confident
        //   pLDDT > 50  → low
        //   pLDDT ≤ 50  → very_low
        assert_eq!(plddt_bucket(95.0), "very_high");
        assert_eq!(plddt_bucket(90.01), "very_high");
        assert_eq!(plddt_bucket(90.0), "confident"); // boundary: not >90
        assert_eq!(plddt_bucket(80.0), "confident");
        assert_eq!(plddt_bucket(70.01), "confident");
        assert_eq!(plddt_bucket(70.0), "low");       // boundary: not >70
        assert_eq!(plddt_bucket(60.0), "low");
        assert_eq!(plddt_bucket(50.01), "low");
        assert_eq!(plddt_bucket(50.0), "very_low");  // boundary: not >50
        assert_eq!(plddt_bucket(20.0), "very_low");
        assert_eq!(plddt_bucket(0.0), "very_low");
    }

    #[test]
    fn af_seed_has_20_proteins_with_consistent_metadata() {
        assert_eq!(AF_SEED.len(), 20);
        for rec in AF_SEED {
            assert!(!rec.name.is_empty());
            assert!(!rec.uniprot.is_empty());
            assert!(rec.seq_len > 0);
            if let Some(mean) = rec.plddt_mean {
                assert!((0.0..=100.0).contains(&mean), "pLDDT out of range for {}: {mean}", rec.name);
                let b = plddt_bucket(mean);
                assert!(
                    matches!(b, "very_high" | "confident" | "low" | "very_low"),
                    "bucket for {} = {b}",
                    rec.name,
                );
            }
        }
        // ATM + BRCA2 are the two known-None entries (AF-DB retired their
        // predictions for proteins >2700 aa as of v6).
        let none_count = AF_SEED.iter().filter(|r| r.plddt_mean.is_none()).count();
        assert_eq!(none_count, 2, "expected exactly 2 proteins with no AF-DB record");
        let none_names: Vec<&str> = AF_SEED
            .iter()
            .filter(|r| r.plddt_mean.is_none())
            .map(|r| r.name)
            .collect();
        assert!(none_names.contains(&"ATM"));
        assert!(none_names.contains(&"BRCA2"));
        // The five structural-biology showcase parents are present too.
        for showcase in ["Trypsin", "TFIIIA", "Insulin", "Myoglobin", "GFP"] {
            assert!(
                AF_SEED.iter().any(|r| r.name == showcase),
                "missing showcase protein {showcase}",
            );
        }
    }

    /// End-to-end engine seed check: every protein carries the AF
    /// properties; every complex hyperedge that can be scored has
    /// `PROP_COMPLEX_CONFIDENCE` attached.
    #[test]
    fn seeded_engine_has_plddt_properties() {
        use ndb_engine::record::Record;

        // Fresh temp DB scoped to this test only — never collides with
        // the example's `/tmp/v22-explorer-ndb` or with parallel tests.
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "v22-explorer-seed-test-{}",
            uuid::Uuid::now_v7().simple()
        ));
        let mut engine = Engine::create(&dir).expect("create");
        seed(&mut engine);
        engine.flush().expect("flush");

        // `TxId::ACTIVE` means "latest visible" — the right snapshot for
        // a freshly-seeded engine with nothing in flight.
        let records: Vec<Record> = engine
            .snapshot_iter_streaming(TxId::ACTIVE)
            .collect::<Result<Vec<_>, _>>()
            .expect("iter");

        // Count protein entities + how many carry the AF properties.
        let mut protein_count = 0_usize;
        let mut with_plddt_mean = 0_usize;
        let mut with_plddt_bucket = 0_usize;
        let mut with_uniprot = 0_usize;
        for r in &records {
            if let Record::Entity(e) = r {
                if e.type_id == TypeId::new(T_PROTEIN) {
                    protein_count += 1;
                    for (pid, _) in &e.properties {
                        if *pid == PropertyId::new(PROP_PLDDT_MEAN) {
                            with_plddt_mean += 1;
                        }
                        if *pid == PropertyId::new(PROP_PLDDT_BUCKET) {
                            with_plddt_bucket += 1;
                        }
                        if *pid == PropertyId::new(PROP_UNIPROT) {
                            with_uniprot += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(protein_count, 20);
        assert_eq!(with_plddt_mean, 18); // ATM + BRCA2 have None
        assert_eq!(with_plddt_bucket, 18);
        assert_eq!(with_uniprot, 20); // UniProt accession known for all 20

        // Complex hyperedges: 6 in the seed; 5 should carry confidence
        // (the "BRCA repair" complex includes ATM → None → skip).
        let mut complex_count = 0_usize;
        let mut with_complex_conf = 0_usize;
        for r in &records {
            if let Record::HyperEdge(h) = r {
                if h.type_id == TypeId::new(T_COMPLEX) {
                    complex_count += 1;
                    if h.properties
                        .iter()
                        .any(|(pid, _)| *pid == PropertyId::new(PROP_COMPLEX_CONFIDENCE))
                    {
                        with_complex_conf += 1;
                    }
                }
            }
        }
        assert_eq!(complex_count, 6);
        // Two complexes touch ATM (p53 surveillance, BRCA repair) and
        // BRCA repair additionally touches BRCA2 — both proteins lack
        // an AF-DB record, so the proxy can't be computed for them.
        // The remaining 4 complexes get PROP_COMPLEX_CONFIDENCE.
        assert_eq!(with_complex_conf, 4);

        // Residue dataset (§C) — verify the totals match the curated
        // tables in residues.rs (which itself has unit tests for each
        // individual protein's motif list).
        let mut residue_count = 0_usize;
        let mut triad_count = 0_usize;
        let mut disulfide_count = 0_usize;
        let mut zinc_finger_count = 0_usize;
        let mut alpha_helix_count = 0_usize;
        let mut beta_sheet_pair_count = 0_usize;
        let mut protein_residues_count = 0_usize;
        let mut protein_residues_total_arity = 0_usize;
        for r in &records {
            match r {
                Record::Entity(e) if e.type_id == TypeId::new(residues::T_RESIDUE) => {
                    residue_count += 1;
                }
                Record::HyperEdge(h) => {
                    let t = h.type_id.get();
                    if t == residues::T_CATALYTIC_TRIAD     { triad_count += 1; }
                    if t == residues::T_DISULFIDE_BOND      { disulfide_count += 1; }
                    if t == residues::T_ZINC_FINGER         { zinc_finger_count += 1; }
                    if t == residues::T_ALPHA_HELIX         { alpha_helix_count += 1; }
                    if t == residues::T_BETA_SHEET_PAIR     { beta_sheet_pair_count += 1; }
                    if t == residues::T_PROTEIN_RESIDUES {
                        protein_residues_count += 1;
                        protein_residues_total_arity += h.roles.len();
                    }
                }
                _ => {}
            }
        }
        // Residue totals per protein:
        //   Trypsin    9  (incl. catalytic Ser/His/Asp + S1 pocket + Cys220)
        //   TFIIIA     4  (Cys-Cys-His-His)
        //   Insulin    6  (six cysteines that form the 3 S-S bonds)
        //   Myoglobin 16  (F-helix residues 80-95)
        //   GFP       43  (β1 + β2 + β3 + β6 strands + chromophore tripeptide)
        // = 78 residues total across 5 showcase proteins.
        assert_eq!(residue_count, 78);
        // ONE N-ary protein_residues hyperedge per showcase protein
        // (5 total), with total arity = 5 protein-fillers + 78
        // residue-fillers = 83. This is the N-ary "contains" pattern
        // — one record per parent, not one per child.
        assert_eq!(protein_residues_count, 5);
        assert_eq!(protein_residues_total_arity, 5 + 78);
        // 1 trypsin triad + 3 insulin disulfides + 1 TFIIIA finger +
        // 1 myoglobin helix + 2 GFP sheet pairs = 8 motif hyperedges.
        assert_eq!(triad_count, 1);
        assert_eq!(disulfide_count, 3);
        assert_eq!(zinc_finger_count, 1);
        assert_eq!(alpha_helix_count, 1);
        assert_eq!(beta_sheet_pair_count, 2);

        // The catalytic triad MUST be arity 3 — that's the whole point.
        for r in &records {
            if let Record::HyperEdge(h) = r {
                if h.type_id == TypeId::new(residues::T_CATALYTIC_TRIAD) {
                    assert_eq!(h.roles.len(), 3,
                        "catalytic_triad must be arity-3 (Ser-His-Asp)");
                }
                if h.type_id == TypeId::new(residues::T_ZINC_FINGER) {
                    assert_eq!(h.roles.len(), 4,
                        "C2H2 zinc finger must be arity-4 (Cys-Cys-His-His)");
                }
            }
        }

        // Best-effort cleanup. Failures here are non-fatal — the tempdir
        // is in $TMPDIR and will get reaped eventually.
        drop(engine);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
