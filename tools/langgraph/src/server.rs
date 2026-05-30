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
/// How many top-cited papers the server pre-ranks once and caches (in RAM +
/// `<db>/top.json`). Covers the default `/view/top` tile, its `as_of`
/// over-fetch, and the per-field `/view/cluster/*` scan from a single
/// O(cache) filter — no live `property_top_k` (the 21.8 s sidecar walk) per
/// request. 20k × ~80 B ≈ 1.6 MB resident / on disk: bounded regardless of
/// graph size.
const CACHE_TOP_N: usize = 20_000;
/// Max incident hyperedges `cites_out` reads per node. Bounds disk reads on
/// power-law hubs (a top-cited paper has ~10^5 incident edges); a viz tile
/// only needs a sparse sample, and normal papers stay under this. See
/// `cites_out`.
const MAX_INCIDENT_SCAN: usize = 256;
/// Cap on internal citation links precomputed AMONG the cached top papers
/// (`load_or_build_top_links`). Computing links live via `cites_out` costs ~1s
/// per top-cited hub (a MAX_INCIDENT_SCAN walk across 204 sidecars), so instead
/// we scan the CITES hyperedges ONCE, keep those whose both endpoints are top
/// papers, and persist them. `tile_cached` then filters this set to the tile's
/// nodes for free — real linkages, O(1) per request.
const MAX_TOP_LINKS: usize = 60_000;
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

/// One entry in the pre-ranked top-cited cache. `field` is already coarsened
/// (so `/view/cluster/<field>` filters with a plain `==`); `year` lets the
/// `as_of` time-travel filter run without a per-entity read.
struct TopEntry {
    eid: EntityId,
    label: String,
    field: String,
    year: i64,
    citations: i64,
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
    top: Vec<TopEntry>,                    // pre-ranked top-cited cache (desc)
    top_links: Vec<(EntityId, EntityId)>,  // precomputed CITES among top papers
    cloud_path: String,                    // path to cloud.bin (all-papers point cloud)
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

/// Pre-rank the top-`CACHE_TOP_N` papers by citations ONCE and cache to
/// `<db>/top.json`. On a compacted DB (one .pidx) this is a single-source
/// top-k (fast); on an uncompacted one it pays the multi-sidecar walk a
/// single time, then every `/view/top` + `/view/cluster/*` request is an
/// O(cache) slice. `top_fields` coarsens each field to match `/view/clusters`.
fn load_or_build_top(engine: &Engine, db: &str, top_fields: &HashSet<String>) -> Vec<TopEntry> {
    let coarse = |f: &str| if top_fields.contains(f) { f.to_string() } else { "Other".to_string() };

    // Fast path: the sidecar written by a previous run (or after --compact,
    // which deletes a stale one). Instant restart at any graph size.
    if let Ok(bytes) = std::fs::read(format!("{db}/top.json"))
        && let Ok(j) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(arr) = j["top"].as_array()
    {
        let top: Vec<TopEntry> = arr.iter().filter_map(|e| {
            let eid = EntityId::from_uuid(e["u"].as_str()?.parse::<uuid::Uuid>().ok()?);
            Some(TopEntry {
                eid,
                label: e["l"].as_str().unwrap_or("").to_string(),
                field: e["f"].as_str().unwrap_or("Other").to_string(),
                year: e["y"].as_i64().unwrap_or(0),
                citations: e["c"].as_i64().unwrap_or(0),
            })
        }).collect();
        if !top.is_empty() {
            eprintln!("top cache: loaded {} pre-ranked papers from top.json", top.len());
            return top;
        }
    }

    // Build via ONE streaming scan + a bounded min-heap of the top
    // CACHE_TOP_N by citations — O(N) single pass, no per-SSTable fan-out and
    // no per-candidate MVCC verify. property_top_k fans out over every sidecar
    // (k candidates each, each then verified with a random read): >16 min and
    // never finished at 204 sidecars (6.17M-paper real DB). The stream yields
    // current entities, so the heap is authoritative without verification.
    eprintln!("top.json missing — pre-ranking top {CACHE_TOP_N} cited via streaming scan (one-time)…");
    let t = std::time::Instant::now();
    let paper_t = TypeId::new(TYPE_PAPER);
    // Reverse → smallest of the current top-K sits on top, ready to evict.
    let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(i64, uuid::Uuid)>> =
        std::collections::BinaryHeap::with_capacity(CACHE_TOP_N + 1);
    for item in engine.snapshot_iter_streaming(TxId::ACTIVE) {
        let Ok(Record::Entity(en)) = item else { continue };
        if en.type_id != paper_t {
            continue;
        }
        let cit = i64_prop(&en.properties, PROP_CITATIONS);
        if heap.len() < CACHE_TOP_N {
            heap.push(std::cmp::Reverse((cit, en.entity_id.into_uuid())));
        } else if heap.peek().is_some_and(|std::cmp::Reverse((m, _))| cit > *m) {
            heap.pop();
            heap.push(std::cmp::Reverse((cit, en.entity_id.into_uuid())));
        }
    }
    // Resolve field/year for just the surviving top-K (bounded re-reads).
    let mut top = Vec::with_capacity(heap.len());
    for std::cmp::Reverse((cit, u)) in heap {
        if let Ok(ndb_engine::Resolved::Live(Record::Entity(en))) =
            engine.snapshot_read(&u, TxId::ACTIVE)
        {
            top.push(TopEntry {
                eid: EntityId::from_uuid(u),
                label: str_prop(&en.properties, PROP_NAME),
                field: coarse(&str_prop(&en.properties, PROP_FIELD)),
                year: i64_prop(&en.properties, PROP_YEAR),
                citations: cit,
            });
        }
    }
    // Heap order isn't sorted — sort citations desc for the served slice.
    top.sort_by(|a, b| b.citations.cmp(&a.citations).then(a.eid.into_uuid().cmp(&b.eid.into_uuid())));

    // Persist atomically (temp + rename) so a kill mid-write never leaves a
    // half-file that load reads as authoritative.
    let arr: Vec<serde_json::Value> = top.iter().map(|t| serde_json::json!({
        "u": t.eid.into_uuid().to_string(), "l": t.label, "f": t.field, "y": t.year, "c": t.citations
    })).collect();
    let payload = serde_json::json!({"version": 1, "n": top.len(), "top": arr});
    let tmp = format!("{db}/top.json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec(&payload).unwrap_or_default())
        .and_then(|()| std::fs::rename(&tmp, format!("{db}/top.json"))).is_err()
    {
        eprintln!("top cache: warning — could not persist top.json (serving from RAM this run)");
    }
    eprintln!("top cache: pre-ranked {} papers in {:.1}s → top.json", top.len(), t.elapsed().as_secs_f64());
    top
}

/// Internal citation links AMONG the cached top papers — precomputed ONCE via a
/// single streaming pass over the CITES hyperedges (NOT per-node `cites_out`,
/// which is ~1s per power-law hub). Keeps each CITES edge whose BOTH endpoints
/// are in `top_set` as (citing, cited). Persisted to `top-links.json` so a
/// restart is instant; capped at `MAX_TOP_LINKS`.
fn load_or_build_top_links(
    engine: &Engine,
    db: &str,
    top_set: &HashSet<EntityId>,
) -> Vec<(EntityId, EntityId)> {
    let path = format!("{db}/top-links.json");
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(j) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(arr) = j["links"].as_array()
    {
        let links: Vec<(EntityId, EntityId)> = arr.iter().filter_map(|e| {
            let s = EntityId::from_uuid(e.get(0)?.as_str()?.parse::<uuid::Uuid>().ok()?);
            let d = EntityId::from_uuid(e.get(1)?.as_str()?.parse::<uuid::Uuid>().ok()?);
            Some((s, d))
        }).collect();
        if !links.is_empty() {
            eprintln!("top-links cache: loaded {} links from top-links.json", links.len());
            return links;
        }
    }
    eprintln!("top-links.json missing — scanning CITES edges among top papers (one-time)…");
    let t0 = std::time::Instant::now();
    let cites_t = TypeId::new(TYPE_CITES);
    let mut links: Vec<(EntityId, EntityId)> = Vec::new();
    for item in engine.snapshot_iter_streaming(TxId::ACTIVE) {
        let Ok(Record::HyperEdge(h)) = item else { continue };
        if h.type_id != cites_t {
            continue;
        }
        let citing = h.roles.iter().find(|(r, _)| r.get() == 30).map(|(_, e)| *e);
        let cited = h.roles.iter().find(|(r, _)| r.get() == 31).map(|(_, e)| *e);
        if let (Some(s), Some(d)) = (citing, cited)
            && s != d
            && top_set.contains(&s)
            && top_set.contains(&d)
        {
            links.push((s, d));
            if links.len() >= MAX_TOP_LINKS {
                break;
            }
        }
    }
    let arr: Vec<serde_json::Value> = links.iter().map(|(s, d)| {
        serde_json::json!([s.into_uuid().to_string(), d.into_uuid().to_string()])
    }).collect();
    let payload = serde_json::json!({"version": 1, "n": links.len(), "links": arr});
    let tmp = format!("{db}/top-links.json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec(&payload).unwrap_or_default())
        .and_then(|()| std::fs::rename(&tmp, &path)).is_err()
    {
        eprintln!("top-links cache: warning — could not persist top-links.json");
    }
    eprintln!("top-links cache: {} links in {:.1}s → top-links.json", links.len(), t0.elapsed().as_secs_f64());
    links
}

/// Build the all-papers point-cloud file `cloud.bin` (one-time, cached): a
/// compact binary of EVERY paper's deterministic position + field + size, for
/// the explorer's GPU `THREE.Points` backdrop (the 3d-force-graph interactive
/// layer maxes out at a few thousand meshes; the cloud renders all ~6M points
/// in one draw call). Layout matches `Index::pos` exactly so cloud + force
/// nodes coincide. Streaming write — bounded RAM regardless of graph size.
///
/// Format (little-endian): header [magic "NDCLOUD1" (8) | u32 count | u32 0],
/// then count × record [f32 x | f32 y | f32 z | u16 field_idx | u16 size_q],
/// 16 bytes/record. `field_idx` indexes the clusters array (explorer maps it to
/// the same field color); `size_q` = importance·65535.
fn build_cloud_file(
    engine: &Engine,
    db: &str,
    clusters: &[(String, usize)],
    cluster_pos: &HashMap<String, (f64, f64)>,
    top_fields: &HashSet<String>,
    max_cit: f64,
    mid_year: f64,
) {
    let path = format!("{db}/cloud.bin");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 16) {
        eprintln!("cloud: cloud.bin present ({} MB)", std::fs::metadata(&path).map(|m| m.len() >> 20).unwrap_or(0));
        return;
    }
    eprintln!("cloud: building cloud.bin (all papers, one-time)…");
    let t0 = std::time::Instant::now();
    let field_idx: HashMap<&str, u16> = clusters.iter().enumerate()
        .map(|(i, (f, _))| (f.as_str(), i as u16)).collect();
    let other_idx = *field_idx.get("Other").unwrap_or(&0);
    let ln_max = (max_cit + 1.0).ln().max(1e-9);
    let tmp = format!("{path}.tmp");
    let Ok(f) = std::fs::File::create(&tmp) else { eprintln!("cloud: cannot create {tmp}"); return };
    let mut w = std::io::BufWriter::new(f);
    let mut buf = [0u8; 16];
    let mut count: u32 = 0;
    // Header written first with count=0, patched after (seek) — but BufWriter
    // can't seek mid-stream cheaply; instead reserve, write records, then
    // rewrite header at the end via a second open. Simpler: write header now
    // with a placeholder and fix count with a final pwrite.
    let _ = w.write_all(b"NDCLOUD1");
    let _ = w.write_all(&0u32.to_le_bytes());
    let _ = w.write_all(&0u32.to_le_bytes());
    for item in engine.snapshot_iter_streaming(TxId::ACTIVE) {
        let Ok(Record::Entity(e)) = item else { continue };
        if e.type_id != TypeId::new(TYPE_PAPER) { continue; }
        let field_raw = str_prop(&e.properties, PROP_FIELD);
        let field = if top_fields.contains(&field_raw) { field_raw.as_str() } else { "Other" };
        let (ax, ay) = *cluster_pos.get(field).unwrap_or(&(0.0, 0.0));
        let cit = i64_prop(&e.properties, PROP_CITATIONS);
        let year = i64_prop(&e.properties, PROP_YEAR);
        let imp = ((cit as f64 + 1.0).ln() / ln_max).clamp(0.0, 1.0);
        let r = SPREAD * (1.0 - imp);
        let uuid_str = e.entity_id.into_uuid().to_string();
        let theta = (hash_str(&uuid_str) % 100_000) as f64 / 100_000.0 * std::f64::consts::TAU;
        let x = (ax + r * theta.cos()) as f32;
        let y = (ay + r * theta.sin()) as f32;
        let z = ((year as f64 - mid_year) * ZSCALE) as f32;
        let fi = *field_idx.get(field).unwrap_or(&other_idx);
        let sq = (imp * 65535.0) as u16;
        buf[0..4].copy_from_slice(&x.to_le_bytes());
        buf[4..8].copy_from_slice(&y.to_le_bytes());
        buf[8..12].copy_from_slice(&z.to_le_bytes());
        buf[12..14].copy_from_slice(&fi.to_le_bytes());
        buf[14..16].copy_from_slice(&sq.to_le_bytes());
        if w.write_all(&buf).is_err() { eprintln!("cloud: write error"); return; }
        count += 1;
    }
    let _ = w.flush();
    drop(w);
    // Patch the count into the header.
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&tmp) {
        use std::io::{Seek, SeekFrom};
        let _ = f.seek(SeekFrom::Start(8));
        let _ = f.write_all(&count.to_le_bytes());
    }
    let _ = std::fs::rename(&tmp, &path);
    eprintln!("cloud: {} points → cloud.bin ({} MB, {:.1}s)", count, (count as u64 * 16) >> 20, t0.elapsed().as_secs_f64());
}

impl Index {
    fn build(mut engine: Engine, db: &str, knn_pref: &str, _cache_bytes: usize) -> Self {
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

        // Pre-ranked top-cited cache: makes /view/top (the default first
        // tile) and /view/cluster/* O(cache) slices instead of a live
        // multi-sidecar property_top_k walk per request.
        let top = load_or_build_top(&engine, db, &top_fields);
        // Precompute internal citation links among the top papers (one CITES
        // hyperedge scan, persisted) so tiles show real linkages without a
        // per-node cites_out fan-out.
        let top_set: HashSet<EntityId> = top.iter().map(|t| t.eid).collect();
        let top_links = load_or_build_top_links(&engine, db, &top_set);
        // All-papers GPU point-cloud backdrop (one-time cloud.bin).
        build_cloud_file(&engine, db, &m.clusters, &cluster_pos, &top_fields, m.max_cit.max(1.0), mid_year);
        let cloud_path = format!("{db}/cloud.bin");

        // Resolve the kNN backend. Three modes:
        //  snapshot/auto — global current-vector .vsnap: ONE mmap'd file,
        //    searched with no sidecar fan-out + no per-candidate MVCC verify
        //    (the O(sidecars×k) wall that made kNN ~15 s at 10 GB). Exact +
        //    bounded + fast. Built once (full scan) + persisted.
        //  approx — in-RAM HNSW: fast, ~95-99% recall, but NOT bounded.
        //  exact — engine brute-force over the per-SSTable .vidx sidecars
        //    (bounded, but O(sidecars×k); the slow path kept for comparison).
        let est_vec_ram = m.total.saturating_mul(EMBED_DIM * 4 + 128);
        let want_snapshot = matches!(knn_pref, "snapshot" | "ondisk" | "auto") && m.total > 0;
        let mut knn_hnsw: Option<Mutex<HnswVectorIndex>> = None;
        let mut knn_mode = "exact-bruteforce";
        if want_snapshot {
            let pid = PropertyId::new(PROP_EMBED);
            let ready = match engine.load_vector_snapshot(pid) {
                Ok(true) => {
                    eprintln!("kNN = snapshot: loaded existing .vsnap (mmap, bounded)");
                    true
                }
                _ => {
                    eprintln!("kNN = snapshot: building global current-vector .vsnap (one-time full scan)…");
                    match engine.build_vector_snapshot(pid) {
                        Ok(n) => {
                            eprintln!("kNN = snapshot: {n} vectors → .vsnap (mmap, bounded, no verify)");
                            n > 0
                        }
                        Err(e) => {
                            eprintln!("kNN = snapshot: build failed ({e}); falling back to exact");
                            false
                        }
                    }
                }
            };
            if ready {
                knn_mode = "ondisk-snapshot";
            }
        } else if knn_pref == "approx" {
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
            knn_hnsw = Some(Mutex::new(h));
            knn_mode = "approx-hnsw";
        } else {
            eprintln!("kNN = exact (engine brute-force over on-disk .vidx sidecars, bounded RAM)");
        }

        Index {
            engine,
            clusters: m.clusters,
            cluster_pos,
            top_fields,
            top,
            top_links,
            cloud_path,
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
    ///
    /// BOUNDED: real citation data is power-law — a top-cited hub has hundreds
    /// of thousands of INCIDENT edges (it's *cited by* that many papers, role
    /// 31). `hyperedges_for_entity` returns all of them, and reading each to
    /// discover the handful that are OUTGOING (role 30 == eid) is the 60 s
    /// `/view/top` wall. Cap the scan at `MAX_INCIDENT_SCAN` edges: a viz tile
    /// only needs a sparse sample of internal links, and normal papers
    /// (degree < cap) are still complete. The cap bounds disk reads per node
    /// regardless of degree.
    fn cites_out(&self, eid: EntityId) -> Vec<EntityId> {
        let mut out = Vec::new();
        for hid in self.engine.hyperedges_for_entity_capped(eid, MAX_INCIDENT_SCAN) {
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

    /// Build a tile from cached TopEntry rows with NO per-node snapshot_read
    /// (label/field/year/citations + deterministic layout all come from the
    /// cache; oaid/doi are lazy-loaded on click). Only the first
    /// TILE_LINK_NODES nodes get internal links computed (each a bounded
    /// adjacency walk), so a top-cited tile of power-law hubs is O(const) — NOT
    /// O(limit × sidecars), which was the 14 s+ /view/top wall at 204 sidecars.
    fn tile_cached(&self, entries: &[&TopEntry]) -> serde_json::Value {
        let set: HashSet<EntityId> = entries.iter().map(|t| t.eid).collect();
        let mut nodes = Vec::with_capacity(entries.len());
        let mut by_uuid: HashMap<EntityId, String> = HashMap::new();
        for t in entries {
            let pv = PaperView {
                uuid: t.eid.into_uuid().to_string(),
                label: t.label.clone(),
                field: t.field.clone(),
                year: t.year,
                citations: t.citations,
                oaid: String::new(),
                doi: String::new(),
            };
            by_uuid.insert(t.eid, pv.uuid.clone());
            nodes.push(self.node_json(&pv));
        }
        // Real internal links from the precomputed top-links set, filtered to
        // this tile's nodes — O(top_links), no per-node cites_out fan-out.
        let mut links = Vec::new();
        for (s, d) in &self.top_links {
            if set.contains(s)
                && set.contains(d)
                && let (Some(src), Some(dst)) = (by_uuid.get(s), by_uuid.get(d))
            {
                links.push(serde_json::json!({"source": src, "target": dst, "kind": "cites"}));
            }
        }
        serde_json::json!({ "nodes": nodes, "links": links })
    }

    fn top_view(&self, limit: usize, as_of: Option<i64>) -> serde_json::Value {
        // O(cache) slice of the pre-ranked top-cited list (already desc) — nodes
        // straight from cache (no per-node reads), links bounded by tile_cached.
        let entries: Vec<&TopEntry> = self.top.iter()
            .filter(|t| as_of.is_none_or(|y| t.year <= y))
            .take(limit)
            .collect();
        let mut v = self.tile_cached(&entries);
        v["meta"] = serde_json::json!({
            "total_papers": self.total_papers, "returned": entries.len(),
            "source": "top-cache", "cache_n": self.top.len()
        });
        v
    }

    fn cluster_papers_view(&self, field: &str, limit: usize, as_of: Option<i64>) -> serde_json::Value {
        // Filter the pre-ranked cache by (already-coarsened) field — O(cache),
        // no live property_top_k. A field's depth is bounded by how many of
        // its papers fall in the global top-CACHE_TOP_N.
        let entries: Vec<&TopEntry> = self.top.iter()
            .filter(|t| t.field == field && as_of.is_none_or(|y| t.year <= y))
            .take(limit)
            .collect();
        let mut v = self.tile_cached(&entries);
        v["meta"] = serde_json::json!({
            "field": field, "returned": entries.len(), "source": "top-cache", "cache_n": self.top.len()
        });
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
        let pid = PropertyId::new(PROP_EMBED);
        let hits = if let Some(h) = &self.knn_hnsw {
            // Approximate: HNSW (embeddings in RAM). Lock for the &mut search.
            h.lock().unwrap().search(pid, &qv, k, Distance::Cosine)
        } else if let Some(r) = self.engine.vector_search_snapshot(pid, &qv, k, Distance::Cosine) {
            // Global current-vector snapshot: one mmap'd file, no fan-out/verify.
            r
        } else {
            // Exact: engine brute-force over the per-SSTable .vidx sidecars.
            self.engine.vector_search(pid, &qv, k, Distance::Cosine)
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

    // Binary endpoint: the all-papers point cloud. Streamed as octet-stream
    // (it's ~16 B × N papers — too big for JSON). Served straight from the
    // mmap-friendly file on disk; the browser parses it into typed arrays.
    if path == "/view/cloud" {
        match std::fs::read(&index.cloud_path) {
            Ok(bytes) => {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                stream.write_all(header.as_bytes())?;
                stream.write_all(&bytes)?;
            }
            Err(_) => {
                let msg = b"cloud.bin not built";
                let header = format!(
                    "HTTP/1.1 404 Not Found\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    msg.len()
                );
                stream.write_all(header.as_bytes())?;
                stream.write_all(msg)?;
            }
        }
        return Ok(());
    }

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
