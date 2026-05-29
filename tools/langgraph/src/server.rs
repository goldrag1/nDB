//! langgraph-server — application-layer "view tile" server for the
//! language-graph demo. Embeds the nDB core (read-only) and serves
//! bounded, zoom-driven slices so the 3D explorer stays fluid over an
//! arbitrarily large graph: the browser only ever receives ≤K nodes per
//! request, no matter how much nDB holds.
//!
//! Layering: the ENGINE is generic (storage + indexes + executor, knows
//! nothing about papers). THIS server is the application — it composes
//! core primitives (type scan, adjacency, vector kNN, MVCC `as_of`) into
//! the query shapes the demo needs, and builds its own citation-sorted
//! index + cluster map at startup (the app optimising its own access
//! without touching the database). Layout is an app concern too: node
//! positions are computed deterministically here from stored properties,
//! so every tile places its nodes at the same global coordinates and the
//! pieces slot together seamlessly as you pan/zoom.
//!
//! Endpoints (all bounded by `limit`, default 500):
//!   GET /health
//!   GET /view/clusters                         far-zoom field galaxies
//!   GET /view/top?limit=K&as_of=Y              global top-cited + their cites
//!   GET /view/cluster/<field>?limit=K&as_of=Y  one field's top-cited
//!   GET /view/neighbors/<uuid>?depth=D&limit=K citation neighborhood (relation+)
//!   GET /view/knn?q=<text>&k=K                 semantic search (vector kNN)
//!
//! Run:
//!   cargo run --release -p langgraph --bin langgraph-server -- \
//!       --db .demo-data/langgraph-ndb --bind 127.0.0.1:8791
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation,
         clippy::cast_sign_loss, clippy::too_many_lines)]

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use ndb_engine::index::Index as IndexTrait; // .apply() to populate the HNSW
use ndb_engine::record::Record;
use ndb_engine::{Distance, Engine, EngineConfig, EntityId, PropertyId, TxId, TypeId, Value};
use ndb_index_vector_hnsw::HnswVectorIndex;

const TYPE_PAPER: u32 = 310;
const TYPE_CITES: u32 = 410;
const PROP_NAME: u32 = 50;
const PROP_EMBED: u32 = 52;
const PROP_YEAR: u32 = 55;
const PROP_CITATIONS: u32 = 56;
const PROP_FIELD: u32 = 57;
const PROP_OAID: u32 = 58;
const PROP_DOI: u32 = 59;
const EMBED_DIM: usize = 16;
const DEFAULT_LIMIT: usize = 500;
const RING: f64 = 850.0;
const SPREAD: f64 = 320.0;
const ZSCALE: f64 = 16.0;

/// A paper's display fields, read on demand from nDB for the ≤K nodes a
/// tile returns — never held in bulk. Built from ONE `snapshot_read`.
struct PaperView {
    uuid: String,
    label: String,
    field: String, // coarse cluster field
    year: i64,
    citations: i64,
    oaid: String,
    doi: String,
}

/// Lean app-layer index: holds ONLY the engine + a tiny cluster aggregate
/// (≤19 fields). Every tile is served from the engine's (on-disk under
/// `--low-memory`) indexes + per-node `snapshot_read` — NO per-paper RAM,
/// so server memory is bounded regardless of graph size. This is the
/// "constant-RAM" form of the view server.
struct Index {
    engine: Engine,
    clusters: Vec<(String, usize)>,        // (coarse field, count), ring order
    cluster_pos: HashMap<String, (f64, f64)>,
    top_fields: HashSet<String>,           // named (non-Other) fields → coarsening
    max_cit: f64,
    mid_year: f64,
    total_papers: usize,
    /// Optional ANN backend for /view/knn. `Some` = approximate (HNSW,
    /// embeddings held in RAM — fast, ~95-99% recall); `None` = exact
    /// (engine.vector_search over the on-disk .vidx — bounded RAM, O(N)).
    /// Chosen by the --knn flag (exact|approx|auto). Mutex because the
    /// crate's search() is &mut (lazy graph build).
    knn_hnsw: Option<Mutex<HnswVectorIndex>>,
    knn_mode: &'static str,
}

fn str_prop(props: &[(PropertyId, Value)], pid: u32) -> String {
    props.iter().find(|(p, _)| p.get() == pid).and_then(|(_, v)| match v {
        Value::String(s) => Some(s.clone()), _ => None }).unwrap_or_default()
}
fn i64_prop(props: &[(PropertyId, Value)], pid: u32) -> i64 {
    props.iter().find(|(p, _)| p.get() == pid).and_then(|(_, v)| match v {
        Value::I64(n) => Some(*n), _ => None }).unwrap_or(0)
}

/// Matches the Rust ingestor + the JS explorer's embed() byte-for-byte.
fn embed(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; EMBED_DIM];
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    for c in &chars {
        v[(*c as u32).wrapping_mul(2_654_435_761) as usize % EMBED_DIM] += 1.0;
    }
    for w in chars.windows(2) {
        let h = (w[0] as u32).wrapping_mul(31).wrapping_add(w[1] as u32)
            .wrapping_mul(2_654_435_761) as usize % EMBED_DIM;
        v[h] += 1.0;
    }
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 { for x in &mut v { *x /= norm; } }
    v
}

fn hash_str(s: &str) -> u64 {
    let mut h = 1469598103934665603u64;
    for b in s.bytes() { h ^= b as u64; h = h.wrapping_mul(1099511628211); }
    h
}

/// Cluster aggregate read from `<db>/clusters.json` (written by the
/// ingestor) — or computed by a bounded streaming scan if absent.
struct ClusterMeta {
    clusters: Vec<(String, usize)>,
    max_cit: f64,
    min_year: i64,
    max_year: i64,
    total: usize,
}

fn load_cluster_meta(engine: &Engine, db: &str) -> ClusterMeta {
    // Fast path: the tiny sidecar written at ingest.
    if let Ok(bytes) = std::fs::read(format!("{db}/clusters.json"))
        && let Ok(j) = serde_json::from_slice::<serde_json::Value>(&bytes)
    {
        let clusters = j["clusters"].as_array().map(|a| a.iter().filter_map(|e| {
            let f = e.get(0)?.as_str()?.to_string();
            let c = e.get(1)?.as_u64()? as usize;
            Some((f, c))
        }).collect()).unwrap_or_default();
        return ClusterMeta {
            clusters,
            max_cit: j["max_cit"].as_f64().unwrap_or(1.0).max(1.0),
            min_year: j["min_year"].as_i64().unwrap_or(2020),
            max_year: j["max_year"].as_i64().unwrap_or(2020),
            total: j["total"].as_u64().unwrap_or(0) as usize,
        };
    }
    // Fallback: bounded streaming scan (field counts only — no per-paper RAM).
    eprintln!("clusters.json missing — computing via one streaming scan");
    let mut fcount: HashMap<String, usize> = HashMap::new();
    let (mut max_cit, mut min_year, mut max_year, mut total) = (1i64, i64::MAX, i64::MIN, 0usize);
    for item in engine.snapshot_iter_streaming(TxId::ACTIVE) {
        let Ok(Record::Entity(e)) = item else { continue };
        if e.type_id != TypeId::new(TYPE_PAPER) { continue; }
        total += 1;
        *fcount.entry(str_prop(&e.properties, PROP_FIELD)).or_default() += 1;
        let c = i64_prop(&e.properties, PROP_CITATIONS);
        if c > max_cit { max_cit = c; }
        let y = i64_prop(&e.properties, PROP_YEAR);
        if y > 0 { min_year = min_year.min(y); max_year = max_year.max(y); }
    }
    let mut fc: Vec<(String, usize)> = fcount.into_iter().collect();
    fc.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top: HashSet<String> = fc.iter().take(18).map(|(f, _)| f.clone()).collect();
    let mut coarse: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (f, c) in &fc { *coarse.entry(if top.contains(f) { f.clone() } else { "Other".into() }).or_default() += c; }
    if min_year == i64::MAX { min_year = 2020; max_year = 2020; }
    ClusterMeta { clusters: coarse.into_iter().collect(), max_cit: max_cit as f64, min_year, max_year, total }
}

impl Index {
    fn build(engine: Engine, db: &str, knn_pref: &str, cache_bytes: usize) -> Self {
        let m = load_cluster_meta(&engine, db);
        let k = m.clusters.len().max(1);
        let mut cluster_pos = HashMap::new();
        for (i, (field, _)) in m.clusters.iter().enumerate() {
            let ang = std::f64::consts::TAU * (i as f64) / (k as f64);
            cluster_pos.insert(field.clone(), (RING * ang.cos(), RING * ang.sin()));
        }
        let top_fields: HashSet<String> =
            m.clusters.iter().map(|(f, _)| f.clone()).filter(|f| f != "Other").collect();
        let mid_year = (m.min_year + m.max_year) as f64 / 2.0;
        eprintln!("served lean: {} papers, {} clusters (no per-paper RAM)", m.total, m.clusters.len());

        // Resolve the kNN backend. Approx (HNSW) loads every embedding into
        // RAM — fast + tunable recall, but NOT bounded — so `auto` only
        // picks it when the vectors comfortably fit the cache budget
        // (vector RAM ≈ N × (dim*4 + ~128 graph bytes); EMBED_DIM is fixed).
        let est_vec_ram = m.total.saturating_mul(EMBED_DIM * 4 + 128);
        let use_approx = match knn_pref {
            "approx" => true,
            "exact" => false,
            _ /* auto */ => m.total > 0 && est_vec_ram <= cache_bytes / 2,
        };
        let (knn_hnsw, knn_mode) = if use_approx {
            eprintln!(
                "kNN = approx (HNSW): loading {} embeddings into RAM (~{} MB est)…",
                m.total, est_vec_ram / 1_048_576
            );
            let mut h = HnswVectorIndex::new();
            h.register_property(PropertyId::new(PROP_EMBED));
            for item in engine.snapshot_iter_streaming(TxId::ACTIVE) {
                if let Ok(rec) = item {
                    h.apply(&rec, TxId::ACTIVE);
                }
            }
            // Trigger the graph build once so the first real query isn't slow.
            let _ = h.search(PropertyId::new(PROP_EMBED), &embed("warmup"), 1, Distance::Cosine);
            eprintln!("kNN = approx (HNSW): graph ready");
            (Some(Mutex::new(h)), "approx-hnsw")
        } else {
            eprintln!("kNN = exact (engine brute-force over on-disk .vidx, bounded RAM)");
            (None, "exact-bruteforce")
        };

        Index {
            engine,
            clusters: m.clusters,
            cluster_pos,
            top_fields,
            max_cit: m.max_cit.max(1.0),
            mid_year,
            total_papers: m.total,
            knn_hnsw,
            knn_mode,
        }
    }

    fn coarse(&self, field: &str) -> String {
        if self.top_fields.contains(field) { field.to_string() } else { "Other".to_string() }
    }

    /// Read one paper's display fields from nDB (one bounded snapshot_read).
    /// `None` if the entity is absent/deleted.
    fn view(&self, eid: EntityId) -> Option<PaperView> {
        let props = match self.engine.snapshot_read(&eid.into_uuid(), TxId::ACTIVE) {
            Ok(ndb_engine::Resolved::Live(Record::Entity(e))) => e.properties,
            _ => return None,
        };
        Some(PaperView {
            uuid: eid.into_uuid().to_string(),
            label: str_prop(&props, PROP_NAME),
            field: self.coarse(&str_prop(&props, PROP_FIELD)),
            year: i64_prop(&props, PROP_YEAR),
            citations: i64_prop(&props, PROP_CITATIONS),
            oaid: str_prop(&props, PROP_OAID),
            doi: str_prop(&props, PROP_DOI),
        })
    }

    fn pos(&self, p: &PaperView) -> (f64, f64, f64) {
        let (ax, ay) = *self.cluster_pos.get(&p.field).unwrap_or(&(0.0, 0.0));
        let imp = (p.citations as f64 + 1.0).ln() / (self.max_cit + 1.0).ln();
        let r = SPREAD * (1.0 - imp);
        let theta = (hash_str(&p.uuid) % 100_000) as f64 / 100_000.0 * std::f64::consts::TAU;
        let z = (p.year as f64 - self.mid_year) * ZSCALE;
        (ax + r * theta.cos(), ay + r * theta.sin(), z)
    }

    fn node_json(&self, p: &PaperView) -> serde_json::Value {
        let (x, y, z) = self.pos(p);
        serde_json::json!({
            "id": p.uuid, "label": p.label, "kind": "paper", "cluster": p.field, "field": p.field,
            "year": p.year, "citations": p.citations, "oaid": p.oaid, "doi": p.doi,
            "x": x, "y": y, "z": z,
        })
    }

    /// Entities this paper CITES (role 30 = citing == eid → role 31 = cited),
    /// via the engine's (on-disk) adjacency index + a snapshot_read per edge.
    fn cites_out(&self, eid: EntityId) -> Vec<EntityId> {
        let mut out = Vec::new();
        for hid in self.engine.hyperedges_for_entity(eid) {
            if let Ok(ndb_engine::Resolved::Live(Record::HyperEdge(h))) =
                self.engine.snapshot_read(&hid.into_uuid(), TxId::ACTIVE)
                && h.type_id == TypeId::new(TYPE_CITES)
            {
                let citing = h.roles.iter().find(|(r, _)| r.get() == 30).map(|(_, e)| *e);
                if citing == Some(eid) {
                    if let Some((_, cited)) = h.roles.iter().find(|(r, _)| r.get() == 31) {
                        out.push(*cited);
                    }
                }
            }
        }
        out
    }

    /// Build the {nodes, links} tile for a set of entity ids: one
    /// snapshot_read per node (bounded by the set size), plus internal CITES
    /// links discovered via the adjacency index.
    fn tile(&self, eids: &[EntityId]) -> serde_json::Value {
        let set: HashSet<EntityId> = eids.iter().copied().collect();
        let mut nodes = Vec::with_capacity(eids.len());
        let mut by_uuid: HashMap<EntityId, String> = HashMap::new();
        for &e in eids {
            if let Some(v) = self.view(e) {
                by_uuid.insert(e, v.uuid.clone());
                nodes.push(self.node_json(&v));
            }
        }
        let mut links = Vec::new();
        for &e in eids {
            if let Some(src) = by_uuid.get(&e) {
                for cited in self.cites_out(e) {
                    if set.contains(&cited)
                        && let Some(dst) = by_uuid.get(&cited)
                    {
                        links.push(serde_json::json!({"source": src, "target": dst, "kind": "cites"}));
                    }
                }
            }
        }
        serde_json::json!({ "nodes": nodes, "links": links })
    }

    // ── endpoint handlers ─────────────────────────────────────────────
    fn clusters_view(&self) -> serde_json::Value {
        let nodes: Vec<_> = self.clusters.iter().map(|(field, count)| {
            let (x, y) = *self.cluster_pos.get(field).unwrap_or(&(0.0, 0.0));
            serde_json::json!({"id": format!("cluster:{field}"), "label": field, "kind": "cluster",
                "field": field, "count": count, "year": 0, "citations": 0, "x": x, "y": y, "z": 0.0})
        }).collect();
        serde_json::json!({ "nodes": nodes, "links": [],
            "meta": {"total_papers": self.total_papers, "clusters": self.clusters.len()} })
    }

    fn top_view(&self, limit: usize, as_of: Option<i64>) -> serde_json::Value {
        // Ordered top-K straight from the engine's citation index — no
        // in-RAM sorted list. Over-fetch to absorb the as_of year filter.
        let fetch = if as_of.is_some() { limit * 3 } else { limit };
        let hits = self.engine.property_top_k(TypeId::new(TYPE_PAPER), PropertyId::new(PROP_CITATIONS), fetch);
        let eids: Vec<EntityId> = hits.iter().copied()
            .filter(|&e| as_of.is_none_or(|y| self.view(e).is_some_and(|v| v.year <= y)))
            .take(limit).collect();
        let mut v = self.tile(&eids);
        v["meta"] = serde_json::json!({"total_papers": self.total_papers, "returned": eids.len()});
        v
    }

    fn cluster_papers_view(&self, field: &str, limit: usize, as_of: Option<i64>) -> serde_json::Value {
        // No (field, citations) compound index — walk the global citation
        // top-K and keep those in this field, capped so it stays bounded.
        let cap = (limit * 40).max(4000);
        let hits = self.engine.property_top_k(TypeId::new(TYPE_PAPER), PropertyId::new(PROP_CITATIONS), cap);
        let mut eids = Vec::new();
        for e in hits {
            if let Some(v) = self.view(e) {
                if v.field == field && as_of.is_none_or(|y| v.year <= y) {
                    eids.push(e);
                    if eids.len() >= limit { break; }
                }
            }
        }
        let mut v = self.tile(&eids);
        v["meta"] = serde_json::json!({"field": field, "returned": eids.len(), "scanned_cap": cap});
        v
    }

    fn neighbors_view(&self, uuid: &str, depth: usize, limit: usize) -> serde_json::Value {
        let Ok(parsed) = uuid.parse::<uuid::Uuid>() else {
            return serde_json::json!({"nodes": [], "links": [], "meta": {"error": "bad uuid"}});
        };
        let start = EntityId::from_uuid(parsed);
        if self.view(start).is_none() {
            return serde_json::json!({"nodes": [], "links": [], "meta": {"error": "not found"}});
        }
        let mut seen: HashSet<EntityId> = HashSet::from([start]);
        let mut frontier = vec![start];
        for _ in 0..depth {
            let mut next = Vec::new();
            for &e in &frontier {
                for cited in self.cites_out(e) {
                    if seen.insert(cited) { next.push(cited); if seen.len() >= limit { break; } }
                }
                if seen.len() >= limit { break; }
            }
            if seen.len() >= limit { break; }
            frontier = next;
        }
        let eids: Vec<EntityId> = seen.into_iter().collect();
        let mut v = self.tile(&eids);
        v["meta"] = serde_json::json!({"root": uuid, "depth": depth, "returned": eids.len()});
        v
    }

    fn knn_view(&self, q: &str, k: usize) -> serde_json::Value {
        let qv = embed(q);
        let hits = match &self.knn_hnsw {
            // Approximate: HNSW (embeddings in RAM). Lock for the &mut search.
            Some(h) => h.lock().unwrap().search(PropertyId::new(PROP_EMBED), &qv, k, Distance::Cosine),
            // Exact: engine brute-force over the on-disk .vidx (bounded RAM).
            None => self.engine.vector_search(PropertyId::new(PROP_EMBED), &qv, k, Distance::Cosine),
        };
        let eids: Vec<EntityId> = hits.iter().map(|(e, _)| *e).collect();
        let mut v = self.tile(&eids);
        v["meta"] = serde_json::json!({"q": q, "returned": eids.len(), "knn_mode": self.knn_mode});
        v
    }
}

// ── tiny HTTP/1.1 surface (std only) ──────────────────────────────────
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str, d: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| d.into());
    let db = arg("--db", ".demo-data/langgraph-ndb");
    let bind = arg("--bind", "127.0.0.1:8791");
    // Low-RAM core: serve the engine's secondary indexes from disk instead
    // of rebuilding them in RAM. Requires the nDB to have been ingested
    // with --low-memory (so the sidecars exist). The app-side `papers` map
    // below is the remaining (smaller) RAM term — a future reduction.
    let low_memory = args.iter().any(|a| a == "--low-memory");
    let cache_mb: usize = arg("--cache-mb", "2048").parse().unwrap_or(2048);
    // kNN backend: exact (engine brute-force, bounded RAM) | approx (HNSW,
    // embeddings in RAM, fast, ~95-99% recall) | auto (approx iff the
    // vectors fit the cache budget, else exact). Default auto.
    let knn = arg("--knn", "auto");

    eprintln!("opening nDB at {db}{}", if low_memory { " (low-memory)" } else { "" });
    let mut engine = if low_memory {
        Engine::open_with_config(&db, EngineConfig::low_memory(cache_mb * 1024 * 1024))?
    } else {
        Engine::open(&db)?
    };
    // Register the vector index for the embedding property, then rebuild so
    // it's populated from the store (registration isn't persisted; rebuild
    // scans memtable + sstables). This is what lets exact kNN run on the
    // engine instead of a RAM copy of every embedding.
    engine.register_vector_property(PropertyId::new(PROP_EMBED));
    engine.register_property_btree(TypeId::new(TYPE_PAPER), PropertyId::new(PROP_CITATIONS));
    engine.rebuild_indexes()?;
    let index = Arc::new(Index::build(engine, &db, &knn, cache_mb * 1024 * 1024));

    let listener = TcpListener::bind(&bind)?;
    eprintln!("langgraph-server on http://{bind}");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => { let idx = Arc::clone(&index); std::thread::spawn(move || { let _ = handle(&idx, s); }); }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

fn handle(index: &Index, mut stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/").to_string();
    // drain headers
    loop { let mut h = String::new(); reader.read_line(&mut h)?; if h == "\r\n" || h.is_empty() { break; } }

    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let qp: HashMap<String, String> = query.split('&').filter_map(|kv| {
        let (k, v) = kv.split_once('=')?; Some((k.to_string(), urldecode(v)))
    }).collect();
    let num = |k: &str, d: usize| qp.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
    let as_of = qp.get("as_of").and_then(|v| v.parse::<i64>().ok());
    let limit = num("limit", DEFAULT_LIMIT).min(2000);

    let body: serde_json::Value = match path {
        "/health" => serde_json::json!({"status": "ok", "papers": index.total_papers}),
        "/view/clusters" => index.clusters_view(),
        "/view/top" => index.top_view(limit, as_of),
        "/view/knn" => index.knn_view(qp.get("q").map_or("", String::as_str), num("k", 8)),
        p if p.starts_with("/view/cluster/") => index.cluster_papers_view(&urldecode(&p["/view/cluster/".len()..]), limit, as_of),
        p if p.starts_with("/view/neighbors/") => index.neighbors_view(&p["/view/neighbors/".len()..], num("depth", 2), limit),
        _ => serde_json::json!({"error": "unknown endpoint"}),
    };
    let payload = serde_json::to_vec(&body).unwrap_or_default();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len());
    stream.write_all(header.as_bytes())?;
    stream.write_all(&payload)?;
    Ok(())
}

fn urldecode(s: &str) -> String {
    let b = s.replace('+', " ");
    let mut out = Vec::new();
    let mut it = b.bytes().peekable();
    while let Some(c) = it.next() {
        if c == b'%' {
            let h: String = (&mut it).take(2).map(|x| x as char).collect();
            if let Ok(n) = u8::from_str_radix(&h, 16) { out.push(n); continue; }
        }
        out.push(c);
    }
    String::from_utf8_lossy(&out).into_owned()
}
