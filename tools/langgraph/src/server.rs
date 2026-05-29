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

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use ndb_engine::record::Record;
use ndb_engine::{Engine, PropertyId, TxId, TypeId, Value};

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

/// Per-paper metadata, loaded once at startup into the app-side index.
struct Paper {
    uuid: String,
    title: String,
    year: i64,
    citations: i64,
    field: String,        // coarse cluster field
    raw_field: String,    // original (for display)
    oaid: String,
    doi: String,
    embedding: Vec<f32>,
}

/// The app-layer index built on top of the generic engine at startup.
/// Loaded once from one nDB scan, then serves every bounded view from its
/// own citation-sorted / field-bucketed / cosine structures — the
/// application optimising its own access on top of the generic core.
struct Index {
    papers: Vec<Paper>,                 // all papers
    by_cit: Vec<usize>,                 // paper indices, citations desc
    by_field: HashMap<String, Vec<usize>>, // coarse field → indices (cit desc)
    by_uuid: HashMap<String, usize>,    // uuid → index
    cite_out: HashMap<usize, Vec<usize>>, // citing → cited (paper indices)
    clusters: Vec<(String, usize)>,     // (field, count), ring order
    cluster_pos: HashMap<String, (f64, f64)>,
    max_cit: f64,
    mid_year: f64,
}

fn str_prop(props: &[(PropertyId, Value)], pid: u32) -> String {
    props.iter().find(|(p, _)| p.get() == pid).and_then(|(_, v)| match v {
        Value::String(s) => Some(s.clone()), _ => None }).unwrap_or_default()
}
fn i64_prop(props: &[(PropertyId, Value)], pid: u32) -> i64 {
    props.iter().find(|(p, _)| p.get() == pid).and_then(|(_, v)| match v {
        Value::I64(n) => Some(*n), _ => None }).unwrap_or(0)
}
fn vec_prop(props: &[(PropertyId, Value)], pid: u32) -> Vec<f32> {
    props.iter().find(|(p, _)| p.get() == pid).and_then(|(_, v)| match v {
        Value::Vector(x) => Some(x.clone()), _ => None }).unwrap_or_default()
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

impl Index {
    fn build(engine: Engine) -> Self {
        // One scan: papers + CITES edges (authors omitted from view tiles).
        let records = engine.snapshot_iter(TxId::ACTIVE).expect("snapshot_iter");
        let mut papers = Vec::new();
        let mut by_uuid = HashMap::new();
        let mut raw_cites: Vec<(String, String)> = Vec::new(); // (citing uuid, cited uuid)

        for r in &records {
            match r {
                Record::Entity(e) if e.type_id == TypeId::new(TYPE_PAPER) => {
                    let uuid = e.entity_id.into_uuid().to_string();
                    let idx = papers.len();
                    by_uuid.insert(uuid.clone(), idx);
                    papers.push(Paper {
                        uuid,
                        title: str_prop(&e.properties, PROP_NAME),
                        year: i64_prop(&e.properties, PROP_YEAR),
                        citations: i64_prop(&e.properties, PROP_CITATIONS),
                        field: str_prop(&e.properties, PROP_FIELD),
                        raw_field: str_prop(&e.properties, PROP_FIELD),
                        oaid: str_prop(&e.properties, PROP_OAID),
                        doi: str_prop(&e.properties, PROP_DOI),
                        embedding: vec_prop(&e.properties, PROP_EMBED),
                    });
                }
                Record::HyperEdge(h) if h.type_id == TypeId::new(TYPE_CITES) => {
                    let mut citing = None; let mut cited = None;
                    for (rid, eid) in &h.roles {
                        if rid.get() == 30 { citing = Some(eid.into_uuid().to_string()); }
                        else if rid.get() == 31 { cited = Some(eid.into_uuid().to_string()); }
                    }
                    if let (Some(a), Some(b)) = (citing, cited) { raw_cites.push((a, b)); }
                }
                _ => {}
            }
        }

        // Coarse clusters: top-18 fields by paper count (+ "Other").
        let mut field_count: HashMap<String, usize> = HashMap::new();
        for p in &papers { *field_count.entry(p.field.clone()).or_default() += 1; }
        let mut fc: Vec<(String, usize)> = field_count.into_iter().collect();
        fc.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let top: std::collections::HashSet<String> = fc.iter().take(18).map(|(f, _)| f.clone()).collect();
        for p in &mut papers {
            if !top.contains(&p.field) { p.field = "Other".to_string(); }
        }

        // Cluster ring positions + counts (ordered for stable angles).
        let mut cl_count: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for p in &papers { *cl_count.entry(p.field.clone()).or_default() += 1; }
        let clusters: Vec<(String, usize)> = cl_count.into_iter().collect();
        let k = clusters.len().max(1);
        let mut cluster_pos = HashMap::new();
        for (i, (field, _)) in clusters.iter().enumerate() {
            let ang = std::f64::consts::TAU * (i as f64) / (k as f64);
            cluster_pos.insert(field.clone(), (RING * ang.cos(), RING * ang.sin()));
        }

        // Sorted indices.
        let mut by_cit: Vec<usize> = (0..papers.len()).collect();
        by_cit.sort_by(|&a, &b| papers[b].citations.cmp(&papers[a].citations));
        let mut by_field: HashMap<String, Vec<usize>> = HashMap::new();
        for &i in &by_cit { by_field.entry(papers[i].field.clone()).or_default().push(i); }

        // Citation adjacency (paper idx → cited paper idxs).
        let mut cite_out: HashMap<usize, Vec<usize>> = HashMap::new();
        for (a, b) in &raw_cites {
            if let (Some(&ia), Some(&ib)) = (by_uuid.get(a), by_uuid.get(b)) {
                cite_out.entry(ia).or_default().push(ib);
            }
        }

        let max_cit = papers.iter().map(|p| p.citations).max().unwrap_or(1).max(1) as f64;
        let years: Vec<i64> = papers.iter().map(|p| p.year).filter(|y| *y > 0).collect();
        let mid_year = if years.is_empty() { 2020.0 }
            else { (years.iter().min().unwrap() + years.iter().max().unwrap()) as f64 / 2.0 };

        eprintln!("indexed {} papers, {} cite-edges, {} clusters",
            papers.len(), cite_out.values().map(Vec::len).sum::<usize>(), clusters.len());
        Index { papers, by_cit, by_field, by_uuid, cite_out, clusters, cluster_pos, max_cit, mid_year }
    }

    /// Deterministic galaxy position for a paper (cluster anchor + offset).
    fn pos(&self, p: &Paper) -> (f64, f64, f64) {
        let (ax, ay) = *self.cluster_pos.get(&p.field).unwrap_or(&(0.0, 0.0));
        let imp = (p.citations as f64 + 1.0).ln() / (self.max_cit + 1.0).ln();
        let r = SPREAD * (1.0 - imp);
        let theta = (hash_str(&p.uuid) % 100_000) as f64 / 100_000.0 * std::f64::consts::TAU;
        let z = (p.year as f64 - self.mid_year) * ZSCALE;
        (ax + r * theta.cos(), ay + r * theta.sin(), z)
    }

    fn node_json(&self, i: usize) -> serde_json::Value {
        let p = &self.papers[i];
        let (x, y, z) = self.pos(p);
        serde_json::json!({
            "id": p.uuid, "label": p.title, "kind": "paper", "cluster": p.field,
            "field": p.raw_field, "year": p.year, "citations": p.citations,
            "oaid": p.oaid, "doi": p.doi, "embedding": p.embedding,
            "x": x, "y": y, "z": z,
        })
    }

    /// CITES edges among a set of paper indices.
    fn internal_cites(&self, set: &std::collections::HashSet<usize>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for &i in set {
            if let Some(cited) = self.cite_out.get(&i) {
                for &j in cited {
                    if set.contains(&j) {
                        out.push(serde_json::json!({"source": self.papers[i].uuid, "target": self.papers[j].uuid, "kind": "cites"}));
                    }
                }
            }
        }
        out
    }

    fn nodes_and_cites(&self, idxs: &[usize]) -> serde_json::Value {
        let set: std::collections::HashSet<usize> = idxs.iter().copied().collect();
        let nodes: Vec<_> = idxs.iter().map(|&i| self.node_json(i)).collect();
        serde_json::json!({ "nodes": nodes, "links": self.internal_cites(&set) })
    }

    // ── endpoint handlers ─────────────────────────────────────────────
    fn clusters_view(&self) -> serde_json::Value {
        let nodes: Vec<_> = self.clusters.iter().map(|(field, count)| {
            let (x, y) = *self.cluster_pos.get(field).unwrap_or(&(0.0, 0.0));
            serde_json::json!({"id": format!("cluster:{field}"), "label": field, "kind": "cluster",
                "field": field, "count": count, "year": 0, "citations": 0, "x": x, "y": y, "z": 0.0})
        }).collect();
        serde_json::json!({ "nodes": nodes, "links": [],
            "meta": {"total_papers": self.papers.len(), "clusters": self.clusters.len()} })
    }

    fn top_view(&self, limit: usize, as_of: Option<i64>) -> serde_json::Value {
        let idxs: Vec<usize> = self.by_cit.iter().copied()
            .filter(|&i| as_of.is_none_or(|y| self.papers[i].year <= y))
            .take(limit).collect();
        let mut v = self.nodes_and_cites(&idxs);
        v["meta"] = serde_json::json!({"total_papers": self.papers.len(), "returned": idxs.len()});
        v
    }

    fn cluster_papers_view(&self, field: &str, limit: usize, as_of: Option<i64>) -> serde_json::Value {
        let idxs: Vec<usize> = self.by_field.get(field).map(|v| v.iter().copied()
            .filter(|&i| as_of.is_none_or(|y| self.papers[i].year <= y))
            .take(limit).collect()).unwrap_or_default();
        let mut v = self.nodes_and_cites(&idxs);
        v["meta"] = serde_json::json!({"field": field, "total_in_field": self.by_field.get(field).map_or(0, Vec::len), "returned": idxs.len()});
        v
    }

    fn neighbors_view(&self, uuid: &str, depth: usize, limit: usize) -> serde_json::Value {
        let Some(&start) = self.by_uuid.get(uuid) else {
            return serde_json::json!({"nodes": [], "links": [], "meta": {"error": "not found"}});
        };
        let mut seen = std::collections::HashSet::from([start]);
        let mut frontier = vec![start];
        for _ in 0..depth {
            let mut next = Vec::new();
            for &i in &frontier {
                if let Some(c) = self.cite_out.get(&i) {
                    for &j in c { if seen.insert(j) { next.push(j); if seen.len() >= limit { break; } } }
                }
                if seen.len() >= limit { break; }
            }
            if seen.len() >= limit { break; }
            frontier = next;
        }
        let idxs: Vec<usize> = seen.into_iter().collect();
        let mut v = self.nodes_and_cites(&idxs);
        v["meta"] = serde_json::json!({"root": uuid, "depth": depth, "returned": idxs.len()});
        v
    }

    fn knn_view(&self, q: &str, k: usize) -> serde_json::Value {
        // App-side cosine over the loaded embeddings — same 16-d space the
        // ingestor + explorer use. Linear scan is microseconds at demo
        // scale; beyond RAM this would call the engine's vector index.
        let qv = embed(q);
        let mut scored: Vec<(usize, f32)> = self.papers.iter().enumerate()
            .filter(|(_, p)| p.embedding.len() == EMBED_DIM)
            .map(|(i, p)| {
                let mut dot = 0f32;
                for j in 0..EMBED_DIM { dot += qv[j] * p.embedding[j]; }
                (i, 1.0 - dot)
            })
            .collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let idxs: Vec<usize> = scored.into_iter().take(k).map(|(i, _)| i).collect();
        let mut v = self.nodes_and_cites(&idxs);
        v["meta"] = serde_json::json!({"q": q, "returned": idxs.len()});
        v
    }
}

// ── tiny HTTP/1.1 surface (std only) ──────────────────────────────────
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str, d: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| d.into());
    let db = arg("--db", ".demo-data/langgraph-ndb");
    let bind = arg("--bind", "127.0.0.1:8791");

    eprintln!("opening nDB at {db}");
    let engine = Engine::open(&db)?;
    let index = Arc::new(Index::build(engine));

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
        "/health" => serde_json::json!({"status": "ok", "papers": index.papers.len()}),
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
