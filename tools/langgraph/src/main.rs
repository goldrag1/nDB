//! langgraph-ingest — build a "language graph" (scholarly knowledge graph)
//! directly inside nDB, Rust-native.
//!
//! The demo corpus is an OpenAlex citation subgraph. OpenAlex metadata is
//! **CC0 (public domain)** — freely redistributable — so `--fetch` pulls a
//! connected slice once and caches it into the repo; every later run (and
//! the 3D explorer) reads that static cache, never a live API.
//!
//! Why a paper graph: it is the richest **5-dimensional** structure a demo
//! can stand on, and every dimension maps to an nDB feature:
//!   1-3. x/y/z force layout over the CITES topology      (adjacency)
//!   4.   publication year → `as_of` time scrubber        (MVCC time-travel)
//!   5.   abstract/title embedding → semantic projection  (vector_search)
//!   (+)  research field → node colour                    (property)
//!   (+)  citation count → node size / centrality         (property)
//!
//! It also exercises the graph shapes relational engines handle worst:
//!   - **N-ary** `AUTHORED` hyperedges (one paper + N author role-fillers),
//!   - **recursive** CITES chains (the `relation+` traversal the bench
//!     already shows nDB beating `WITH RECURSIVE` on).
//!
//! Writes go straight through the embedded engine (`begin_write` /
//! `put_entity` / `put_hyperedge`) — no seed.json hop, no Python scraper.
//! One transaction per publication year, oldest first, so `as_of(year)`
//! shows the field as it stood that year.
//!
//! Run:
//!     cargo run -p langgraph -- --fetch            # OpenAlex → cached JSON
//!     cargo run -p langgraph                       # ingest cache → .demo-data/langgraph-ndb
//!     cargo run -p langgraph -- /tmp/lg            # custom db dir
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation,
         clippy::cast_sign_loss, clippy::too_many_lines)]

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use ndb_engine::record::{
    EntityRecord, HyperEdgeRecord, PropertyKeyRecord, Record, RoleNameRecord, TypeNameRecord,
};
use ndb_engine::{
    Distance, Engine, EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId, Value,
};

// ─── Schema ────────────────────────────────────────────────────────────
const TYPE_PAPER: u32 = 310;
const TYPE_AUTHOR: u32 = 320;
const TYPE_CITES: u32 = 410; // binary hyperedge: citing → cited
const TYPE_AUTHORED: u32 = 420; // N-ary hyperedge: paper + N authors

const PROP_NAME: u32 = 50; // lookup-key indexed (paper title / author name)
const PROP_KIND: u32 = 51; // "paper" | "author"
const PROP_EMBED: u32 = 52; // vector indexed
const PROP_YEAR: u32 = 55;
const PROP_CITATIONS: u32 = 56;
const PROP_FIELD: u32 = 57; // property-btree indexed (top concept)
const PROP_OAID: u32 = 58; // OpenAlex work id (e.g. W2163605009) — "see more" link
const PROP_DOI: u32 = 59; // full DOI URL or "" — "read the paper" link

const ROLE_CITING: u32 = 30;
const ROLE_CITED: u32 = 31;
const ROLE_PAPER: u32 = 32;
const ROLE_AUTHOR: u32 = 33;

const EMBED_DIM: usize = 16;
const MAX_AUTHORS: usize = 8; // cap arity so AUTHORED stays readable
const CACHE_PATH: &str = "tools/langgraph/data/openalex-ml.json";

// ─── Cached corpus shape (committed JSON; CC0 metadata) ─────────────────
#[derive(Serialize, Deserialize, Clone)]
struct Paper {
    id: String,
    title: String,
    year: i64,
    citations: i64,
    field: String,
    /// Full DOI URL (https://doi.org/…) or "" — the "read the paper" link.
    #[serde(default)]
    doi: String,
    authors: Vec<String>,
    /// Ids of cited papers — kept only when the target is also in the set,
    /// so the cache is an internally-connected subgraph.
    cites: Vec<String>,
}

// ─── Deterministic embedding (offline, reproducible) ────────────────────
// Char unigram + bigram hashing into EMBED_DIM buckets, L2-normalised.
// Fed title + field, so papers sharing a field/topic land near each other
// — enough topical signal for the kNN demo without an embedding API.
fn embed(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; EMBED_DIM];
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    for c in &chars {
        let h = (*c as u32).wrapping_mul(2_654_435_761) as usize % EMBED_DIM;
        v[h] += 1.0;
    }
    for w in chars.windows(2) {
        let h = (w[0] as u32)
            .wrapping_mul(31)
            .wrapping_add(w[1] as u32)
            .wrapping_mul(2_654_435_761) as usize
            % EMBED_DIM;
        v[h] += 1.0;
    }
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

// ─── OpenAlex fetch (ureq; CC0 data) ────────────────────────────────────
fn short_id(openalex_url: &str) -> String {
    openalex_url.rsplit('/').next().unwrap_or(openalex_url).to_string()
}

/// Pull the top-cited connected slice of a topic into `Vec<Paper>`. Keeps
/// only CITES edges whose target is also in the slice, so the result is an
/// internally-connected citation subgraph.
fn fetch_openalex(query: &str, target: usize) -> Result<Vec<Paper>, Box<dyn std::error::Error>> {
    let ua = "langgraph-demo (mailto:demo@nDB.example)";
    let filter = format!("default.search:{query},from_publication_date:2012-01-01");
    let select = "id,doi,display_name,publication_year,cited_by_count,referenced_works,concepts,authorships";

    // OpenAlex caps a page at 200; cursor-paginate to reach `target`. (The
    // 200-per-page cap is the only reason the first cut of this demo had
    // exactly 200 papers — nДB itself has no such limit.)
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut cursor = "*".to_string();
    while results.len() < target {
        let resp: serde_json::Value = ureq::get("https://api.openalex.org/works")
            .query("filter", &filter)
            .query("sort", "cited_by_count:desc")
            .query("per-page", "200")
            .query("cursor", &cursor)
            .query("select", select)
            .set("User-Agent", ua)
            .call()?
            .into_json()?;
        let page = resp["results"].as_array().cloned().unwrap_or_default();
        if page.is_empty() {
            break;
        }
        results.extend(page);
        match resp["meta"]["next_cursor"].as_str() {
            Some(c) if !c.is_empty() => cursor = c.to_string(),
            _ => break,
        }
    }
    results.truncate(target);
    // First pass: collect the id set so we can intersect references.
    let id_set: std::collections::HashSet<String> = results
        .iter()
        .filter_map(|w| w["id"].as_str().map(short_id))
        .collect();

    let mut papers = Vec::new();
    for w in &results {
        let Some(id) = w["id"].as_str().map(short_id) else { continue };
        let title = w["display_name"].as_str().unwrap_or("(untitled)").to_string();
        let year = w["publication_year"].as_i64().unwrap_or(0);
        let citations = w["cited_by_count"].as_i64().unwrap_or(0);
        // First concept whose level >= 1 reads as a usable field label.
        let field = w["concepts"]
            .as_array()
            .and_then(|cs| {
                cs.iter()
                    .find(|c| c["level"].as_i64().unwrap_or(0) >= 1)
                    .or_else(|| cs.first())
            })
            .and_then(|c| c["display_name"].as_str())
            .unwrap_or("Unknown")
            .to_string();
        let authors: Vec<String> = w["authorships"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x["author"]["display_name"].as_str().map(str::to_string))
                    .take(MAX_AUTHORS)
                    .collect()
            })
            .unwrap_or_default();
        let cites: Vec<String> = w["referenced_works"]
            .as_array()
            .map(|r| {
                r.iter()
                    .filter_map(|x| x.as_str().map(short_id))
                    .filter(|rid| id_set.contains(rid) && rid != &id)
                    .collect()
            })
            .unwrap_or_default();
        if year == 0 {
            continue;
        }
        let doi = w["doi"].as_str().unwrap_or("").to_string();
        papers.push(Paper { id, title, year, citations, field, doi, authors, cites });
    }
    Ok(papers)
}

fn load_cache() -> Option<Vec<Paper>> {
    let bytes = std::fs::read(CACHE_PATH).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ─── Schema registration + ingest ──────────────────────────────────────
fn register_schema(engine: &mut Engine) {
    engine.register_lookup_key(PropertyId::new(PROP_NAME));
    engine.register_property_btree(TypeId::new(TYPE_PAPER), PropertyId::new(PROP_FIELD));
    engine.register_vector_property(PropertyId::new(PROP_EMBED));

    let mut tx = engine.begin_write();
    for (id, name) in [
        (TYPE_PAPER, "paper"),
        (TYPE_AUTHOR, "author"),
        (TYPE_CITES, "cites"),
        (TYPE_AUTHORED, "authored"),
    ] {
        tx.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(id), name: name.into() }));
    }
    for (id, name) in [
        (ROLE_CITING, "citing"),
        (ROLE_CITED, "cited"),
        (ROLE_PAPER, "paper"),
        (ROLE_AUTHOR, "author"),
    ] {
        tx.put_raw(Record::RoleName(RoleNameRecord { id: RoleId::new(id), name: name.into() }));
    }
    for (id, name) in [
        (PROP_NAME, "name"),
        (PROP_KIND, "kind"),
        (PROP_EMBED, "embedding"),
        (PROP_YEAR, "year"),
        (PROP_CITATIONS, "citations"),
        (PROP_FIELD, "field"),
        (PROP_OAID, "openalex_id"),
        (PROP_DOI, "doi"),
    ] {
        tx.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(id), name: name.into() }));
    }
    tx.commit().unwrap();
}

fn ensure_author(
    tx: &mut ndb_engine::WriteTxn<'_>,
    authors: &mut HashMap<String, EntityId>,
    name: &str,
) -> EntityId {
    if let Some(eid) = authors.get(name) {
        return *eid;
    }
    let eid = EntityId::now_v7();
    tx.put_entity(EntityRecord {
        entity_id: eid,
        type_id: TypeId::new(TYPE_AUTHOR),
        tx_id_assert: TxId::new(0),
        tx_id_supersede: TxId::ACTIVE,
        properties: vec![
            (PropertyId::new(PROP_NAME), Value::String(name.to_string())),
            (PropertyId::new(PROP_KIND), Value::String("author".into())),
            (PropertyId::new(PROP_EMBED), Value::Vector(embed(name))),
        ],
    });
    authors.insert(name.to_string(), eid);
    eid
}

struct Ingested {
    papers: usize,
    authors: usize,
    cites: usize,
    authored: usize,
    /// (year, committed tx) ascending — the time-travel timeline.
    timeline: Vec<(i64, TxId)>,
    /// OpenAlex id → entity id (consumed by the explorer wiring + tests).
    #[allow(dead_code)]
    paper_ids: HashMap<String, EntityId>,
}

/// Ingest a paper set, one transaction per publication year (oldest
/// first). A paper, its authors, its AUTHORED edge, and its CITES edges to
/// already-created targets all land in that year's tx — so `as_of(year)`
/// is an honest snapshot of the field at that year.
fn ingest_papers(engine: &mut Engine, papers: &[Paper]) -> Ingested {
    let mut by_year: BTreeMap<i64, Vec<&Paper>> = BTreeMap::new();
    for p in papers {
        by_year.entry(p.year).or_default().push(p);
    }

    let mut paper_ids: HashMap<String, EntityId> = HashMap::new();
    let mut author_ids: HashMap<String, EntityId> = HashMap::new();
    let mut timeline = Vec::new();
    let (mut n_cites, mut n_authored) = (0usize, 0usize);
    // Flush the memtable to an SSTable every ~50k records so ingest RAM
    // stays bounded — without this the whole graph lives in the memtable
    // and a large (multi-GB) ingest OOMs before it finishes.
    let mut since_flush = 0usize;

    for (year, group) in by_year {
        let mut tx = engine.begin_write();
        let cites0 = n_cites;
        for p in &group {
            // Paper entity (dedup by OpenAlex id).
            let pid = *paper_ids.entry(p.id.clone()).or_insert_with(EntityId::now_v7);
            tx.put_entity(EntityRecord {
                entity_id: pid,
                type_id: TypeId::new(TYPE_PAPER),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                properties: vec![
                    (PropertyId::new(PROP_NAME), Value::String(p.title.clone())),
                    (PropertyId::new(PROP_KIND), Value::String("paper".into())),
                    (PropertyId::new(PROP_YEAR), Value::I64(p.year)),
                    (PropertyId::new(PROP_CITATIONS), Value::I64(p.citations)),
                    (PropertyId::new(PROP_FIELD), Value::String(p.field.clone())),
                    (PropertyId::new(PROP_OAID), Value::String(p.id.clone())),
                    (PropertyId::new(PROP_DOI), Value::String(p.doi.clone())),
                    (PropertyId::new(PROP_EMBED), Value::Vector(embed(&format!("{} {}", p.title, p.field)))),
                ],
            });

            // N-ary AUTHORED: paper + each author.
            if !p.authors.is_empty() {
                let mut roles = vec![(RoleId::new(ROLE_PAPER), pid)];
                for a in &p.authors {
                    let aid = ensure_author(&mut tx, &mut author_ids, a);
                    roles.push((RoleId::new(ROLE_AUTHOR), aid));
                }
                tx.put_hyperedge(HyperEdgeRecord {
                    hyperedge_id: HyperedgeId::now_v7(),
                    type_id: TypeId::new(TYPE_AUTHORED),
                    tx_id_assert: TxId::new(0),
                    tx_id_supersede: TxId::ACTIVE,
                    roles,
                    hyperedge_roles: Vec::new(),
                    properties: vec![],
                });
                n_authored += 1;
            }

            // CITES edges to targets already created (older or same-year-
            // earlier papers). Forward refs are skipped — citations point
            // backward in time.
            for cited in &p.cites {
                if let Some(&cid) = paper_ids.get(cited) {
                    tx.put_hyperedge(HyperEdgeRecord {
                        hyperedge_id: HyperedgeId::now_v7(),
                        type_id: TypeId::new(TYPE_CITES),
                        tx_id_assert: TxId::new(0),
                        tx_id_supersede: TxId::ACTIVE,
                        roles: vec![
                            (RoleId::new(ROLE_CITING), pid),
                            (RoleId::new(ROLE_CITED), cid),
                        ],
                        hyperedge_roles: Vec::new(),
                        properties: vec![],
                    });
                    n_cites += 1;
                }
            }
        }
        let tx_id = tx.commit().unwrap();
        timeline.push((year, tx_id));
        // ~5 records/paper (entity + authors + authored) + cites added.
        since_flush += group.len() * 5 + (n_cites - cites0);
        if since_flush >= 50_000 {
            engine.flush().unwrap();   // promote memtable → SSTable, free RAM
            since_flush = 0;
        }
    }
    engine.flush().unwrap();           // final flush

    Ingested {
        papers: paper_ids.len(),
        authors: author_ids.len(),
        cites: n_cites,
        authored: n_authored,
        timeline,
        paper_ids,
    }
}

/// Export the whole graph to a viz-ready JSON the static 3D explorer
/// reads. nDB produces the feed: nodes carry the five demo dimensions
/// (year / field / citations / embedding + kind); links flatten CITES
/// (paper→paper) and the N-ary AUTHORED edge (paper→each author) so a
/// force layout can render them. Authors inherit the earliest year of
/// any paper they wrote, so the time scrubber reveals them with their
/// debut.
fn export_graph(engine: &Engine, path: &str) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    use serde_json::json;
    let records = engine.snapshot_iter(TxId::ACTIVE)?;

    let uuid = |e: EntityId| e.into_uuid().to_string();
    let str_prop = |props: &[(PropertyId, Value)], pid: u32| -> Option<String> {
        props.iter().find(|(p, _)| p.get() == pid).and_then(|(_, v)| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
    };
    let i64_prop = |props: &[(PropertyId, Value)], pid: u32| -> i64 {
        props.iter().find(|(p, _)| p.get() == pid).and_then(|(_, v)| match v {
            Value::I64(n) => Some(*n),
            _ => None,
        }).unwrap_or(0)
    };
    let vec_prop = |props: &[(PropertyId, Value)], pid: u32| -> Vec<f32> {
        props.iter().find(|(p, _)| p.get() == pid).and_then(|(_, v)| match v {
            Value::Vector(x) => Some(x.clone()),
            _ => None,
        }).unwrap_or_default()
    };

    // author id → display name; paper id → year.
    let author_name: HashMap<String, String> = records.iter().filter_map(|r| match r {
        Record::Entity(e) if e.type_id == TypeId::new(TYPE_AUTHOR) =>
            Some((uuid(e.entity_id), str_prop(&e.properties, PROP_NAME).unwrap_or_default())),
        _ => None,
    }).collect();
    let paper_year: HashMap<String, i64> = records.iter().filter_map(|r| match r {
        Record::Entity(e) if e.type_id == TypeId::new(TYPE_PAPER) =>
            Some((uuid(e.entity_id), i64_prop(&e.properties, PROP_YEAR))),
        _ => None,
    }).collect();

    // Walk AUTHORED once: per-paper author ids, per-author paper count,
    // per-author debut year. nDB stores every author; the VIZ renders only
    // "bridge" authors (≥2 papers in the set) — the co-authorship structure
    // that actually connects the graph — while a paper's details panel
    // still lists all of its authors by name. Without this, ~4.5k
    // single-paper leaf authors swamp the force layout.
    let mut paper_authors: HashMap<String, Vec<String>> = HashMap::new(); // paper → author ids
    let mut author_count: HashMap<String, usize> = HashMap::new();
    let mut author_year: HashMap<String, i64> = HashMap::new();
    for r in &records {
        if let Record::HyperEdge(h) = r {
            if h.type_id != TypeId::new(TYPE_AUTHORED) { continue; }
            let paper = h.roles.iter().find(|(r, _)| r.get() == ROLE_PAPER).map(|(_, e)| uuid(*e));
            let Some(paper) = paper else { continue };
            let py = paper_year.get(&paper).copied().unwrap_or(0);
            for (rid, aid) in &h.roles {
                if rid.get() != ROLE_AUTHOR { continue; }
                let a = uuid(*aid);
                paper_authors.entry(paper.clone()).or_default().push(a.clone());
                *author_count.entry(a.clone()).or_default() += 1;
                let y = author_year.entry(a).or_insert(i64::MAX);
                if py > 0 { *y = (*y).min(py); }
            }
        }
    }
    let is_bridge = |aid: &str| author_count.get(aid).copied().unwrap_or(0) >= 2;

    let mut nodes = Vec::new();
    let mut links = Vec::new();

    // Nodes: every paper (carrying the full author-name list for its
    // details panel) + only bridge-author entities.
    for r in &records {
        if let Record::Entity(e) = r {
            let id = uuid(e.entity_id);
            if e.type_id == TypeId::new(TYPE_PAPER) {
                let authors: Vec<String> = paper_authors.get(&id).map(|ids|
                    ids.iter().filter_map(|a| author_name.get(a).cloned()).collect()).unwrap_or_default();
                nodes.push(json!({
                    "id": id,
                    "label": str_prop(&e.properties, PROP_NAME).unwrap_or_default(),
                    "kind": "paper",
                    "year": i64_prop(&e.properties, PROP_YEAR),
                    "field": str_prop(&e.properties, PROP_FIELD).unwrap_or_else(|| "Unknown".into()),
                    "citations": i64_prop(&e.properties, PROP_CITATIONS),
                    "oaid": str_prop(&e.properties, PROP_OAID).unwrap_or_default(),
                    "doi": str_prop(&e.properties, PROP_DOI).unwrap_or_default(),
                    "authors": authors,
                    "embedding": vec_prop(&e.properties, PROP_EMBED),
                }));
            } else if e.type_id == TypeId::new(TYPE_AUTHOR) && is_bridge(&id) {
                let y = author_year.get(&id).copied().unwrap_or(0);
                nodes.push(json!({
                    "id": id,
                    "label": str_prop(&e.properties, PROP_NAME).unwrap_or_default(),
                    "kind": "author",
                    "year": if y == i64::MAX { 0 } else { y },
                    "field": "Author",
                    "citations": 0,
                    "embedding": vec_prop(&e.properties, PROP_EMBED),
                }));
            }
        }
    }

    // Links: all CITES; AUTHORED only to bridge authors (the only ones
    // with a node).
    for r in &records {
        if let Record::HyperEdge(h) = r {
            let role = |rid: u32| h.roles.iter().find(|(r, _)| r.get() == rid).map(|(_, e)| uuid(*e));
            if h.type_id == TypeId::new(TYPE_CITES) {
                if let (Some(s), Some(t)) = (role(ROLE_CITING), role(ROLE_CITED)) {
                    links.push(json!({"source": s, "target": t, "kind": "cites"}));
                }
            } else if h.type_id == TypeId::new(TYPE_AUTHORED) {
                if let Some(paper) = role(ROLE_PAPER) {
                    for (rid, aid) in &h.roles {
                        if rid.get() == ROLE_AUTHOR {
                            let a = uuid(*aid);
                            if is_bridge(&a) {
                                links.push(json!({"source": paper.clone(), "target": a, "kind": "authored"}));
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Cluster super-nodes (Step 2 / far-zoom tier) ──────────────────
    // Coarsen fields to the top-K by paper count (rest → "Other"), make one
    // fixed super-node per coarse field on a ring, and add invisible
    // paper→cluster "member" springs so the force layout pulls papers into
    // field "galaxies". When zoomed out the explorer shows only these
    // clusters; zoom in and they give way to the papers. This is what lets
    // the view stay bounded — and meaningful — at any nДB size.
    use std::collections::BTreeMap;
    let mut field_count: HashMap<String, usize> = HashMap::new();
    for n in &nodes {
        if n["kind"] == "paper" {
            *field_count.entry(n["field"].as_str().unwrap_or("Unknown").to_string()).or_default() += 1;
        }
    }
    let mut fc: Vec<(String, usize)> = field_count.into_iter().collect();
    fc.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top_fields: std::collections::HashSet<String> = fc.iter().take(18).map(|(f, _)| f.clone()).collect();
    let coarse = |f: &str| if top_fields.contains(f) { f.to_string() } else { "Other".to_string() };

    let mut cluster_count: BTreeMap<String, usize> = BTreeMap::new();
    for n in &nodes {
        if n["kind"] == "paper" {
            *cluster_count.entry(coarse(n["field"].as_str().unwrap_or("Unknown"))).or_default() += 1;
        }
    }
    let k = cluster_count.len().max(1);
    let ring = 850.0_f64;
    for (i, (field, count)) in cluster_count.iter().enumerate() {
        let ang = std::f64::consts::TAU * (i as f64) / (k as f64);
        nodes.push(json!({
            "id": format!("cluster:{field}"), "label": field, "kind": "cluster",
            "field": field, "count": count, "year": 0, "citations": 0,
            "fx": ring * ang.cos(), "fy": ring * ang.sin(), "fz": 0.0,
        }));
    }
    // Tag papers with their coarse cluster (for colour) + emit member springs.
    for n in &mut nodes {
        if n["kind"] == "paper" {
            let f = coarse(n["field"].as_str().unwrap_or("Unknown"));
            n["cluster"] = json!(f);
            links.push(json!({"source": n["id"].clone(), "target": format!("cluster:{f}"), "kind": "member"}));
        }
    }

    let papers_n = nodes.iter().filter(|n| n["kind"] == "paper").count();
    let authors_shown = nodes.iter().filter(|n| n["kind"] == "author").count();
    let doc = json!({
        "nodes": nodes,
        "links": links,
        // The viz renders a sample; nDB holds the full set. Surfaced so the
        // UI can say "514 of 4,583 authors" honestly.
        "meta": { "papers": papers_n, "authors_total": author_name.len(), "authors_shown": authors_shown },
    });
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec(&doc)?)?;
    Ok((doc["nodes"].as_array().map_or(0, Vec::len), doc["links"].as_array().map_or(0, Vec::len)))
}

/// Count `PAPER` entities visible at a snapshot — the time-travel probe.
fn count_papers_at(engine: &Engine, tx: TxId) -> usize {
    engine
        .snapshot_iter(tx)
        .unwrap()
        .iter()
        .filter(|r| matches!(r, Record::Entity(e) if e.type_id == TypeId::new(TYPE_PAPER)))
        .count()
}

// ─── Tiny network-free corpus for tests + offline fallback ──────────────
fn synthetic_papers() -> Vec<Paper> {
    let mk = |id: &str, title: &str, year: i64, cites_n: i64, field: &str, authors: &[&str], cites: &[&str]| Paper {
        id: id.into(),
        title: title.into(),
        year,
        citations: cites_n,
        field: field.into(),
        doi: String::new(),
        authors: authors.iter().map(|s| (*s).to_string()).collect(),
        cites: cites.iter().map(|s| (*s).to_string()).collect(),
    };
    vec![
        mk("W1", "A Neural Probabilistic Language Model", 2012, 9000, "NLP", &["Bengio", "Ducharme"], &[]),
        mk("W2", "ImageNet Classification with Deep CNNs", 2012, 90000, "Computer Vision", &["Krizhevsky", "Sutskever", "Hinton"], &["W1"]),
        mk("W3", "Sequence to Sequence Learning", 2014, 30000, "NLP", &["Sutskever", "Vinyals", "Le"], &["W1", "W2"]),
        mk("W4", "Deep Residual Learning", 2016, 220000, "Computer Vision", &["He", "Zhang", "Ren", "Sun"], &["W2"]),
        mk("W5", "Attention Is All You Need", 2017, 130000, "NLP", &["Vaswani", "Shazeer", "Parmar"], &["W1", "W3", "W4"]),
        mk("W6", "BERT", 2019, 80000, "NLP", &["Devlin", "Chang", "Lee", "Toutanova"], &["W3", "W5"]),
    ]
}

fn demo_reads(engine: &Engine, ing: &Ingested) {
    println!(
        "ingested {} papers + {} authors, {} CITES + {} AUTHORED edges across {} years",
        ing.papers, ing.authors, ing.cites, ing.authored, ing.timeline.len()
    );

    // (5) semantic kNN — the GraphRAG retrieval dimension.
    let id_to_title: HashMap<EntityId, String> = engine
        .snapshot_iter(TxId::ACTIVE)
        .unwrap()
        .into_iter()
        .filter_map(|r| match r {
            Record::Entity(e) if e.type_id == TypeId::new(TYPE_PAPER) => {
                let t = e.properties.iter().find(|(p, _)| p.get() == PROP_NAME)
                    .and_then(|(_, v)| if let Value::String(s) = v { Some(s.clone()) } else { None })?;
                Some((e.entity_id, t))
            }
            _ => None,
        })
        .collect();
    let probe = "transformer attention language model";
    let hits = engine.vector_search(PropertyId::new(PROP_EMBED), &embed(probe), 5, Distance::Cosine);
    println!("\nsemantic kNN nearest to \"{probe}\":");
    for (eid, dist) in hits {
        if let Some(t) = id_to_title.get(&eid) {
            println!("  {dist:.4}  {}", &t[..t.len().min(60)]);
        }
    }

    // (4) time travel — papers visible at the earliest vs latest year.
    if let (Some(first), Some(last)) = (ing.timeline.first(), ing.timeline.last()) {
        println!(
            "\ntime travel: {} papers as_of {} → {} papers as_of {}",
            count_papers_at(engine, first.1), first.0,
            count_papers_at(engine, last.1), last.0
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("--fetch") {
        let target = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
        let papers = fetch_openalex("deep learning neural network transformer", target)?;
        let connected = papers.iter().filter(|p| !p.cites.is_empty()).count();
        std::fs::create_dir_all("tools/langgraph/data")?;
        std::fs::write(CACHE_PATH, serde_json::to_vec_pretty(&papers)?)?;
        println!(
            "fetched {} papers ({} with internal citations) → {CACHE_PATH}",
            papers.len(), connected
        );
        return Ok(());
    }

    let db_dir = args.first().cloned().unwrap_or_else(|| ".demo-data/langgraph-ndb".to_string());
    let _ = std::fs::remove_dir_all(&db_dir);
    std::fs::create_dir_all(&db_dir)?;

    let papers = load_cache().unwrap_or_else(|| {
        eprintln!("no cache at {CACHE_PATH} — run `--fetch` first; using synthetic fallback");
        synthetic_papers()
    });

    let mut engine = Engine::create(&db_dir)?;
    register_schema(&mut engine);
    let ing = ingest_papers(&mut engine, &papers);
    println!("→ {db_dir}");
    demo_reads(&engine, &ing);

    // Emit the viz feed for the static 3D explorer (docs/langgraph/).
    let (n, l) = export_graph(&engine, "docs/langgraph/graph.json")?;
    println!("\nexported {n} nodes + {l} links → docs/langgraph/graph.json");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_engine() -> (Engine, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "langgraph-test-{}-{}",
            std::process::id(),
            EntityId::now_v7().into_uuid().simple()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut engine = Engine::create(&dir).unwrap();
        register_schema(&mut engine);
        (engine, dir)
    }

    #[test]
    fn ingest_builds_citation_and_authorship_graph() {
        let (mut engine, dir) = temp_engine();
        let papers = synthetic_papers();
        let ing = ingest_papers(&mut engine, &papers);

        assert_eq!(ing.papers, 6, "every paper becomes an entity");
        assert!(ing.authors >= 12, "deduped authors across papers");
        assert!(ing.cites >= 6, "internal CITES edges preserved");
        assert_eq!(ing.authored, 6, "one N-ary AUTHORED edge per paper");
        assert!(ing.paper_ids.contains_key("W5"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn vector_knn_returns_self_first() {
        let (mut engine, dir) = temp_engine();
        let papers = synthetic_papers();
        let _ = ingest_papers(&mut engine, &papers);
        // Query with the exact embed text used at ingest for W5.
        let probe = format!("{} {}", "Attention Is All You Need", "NLP");
        let hits = engine.vector_search(PropertyId::new(PROP_EMBED), &embed(&probe), 3, Distance::Cosine);
        assert!(!hits.is_empty());
        assert!(hits[0].1 < 1e-4, "self distance ~0, got {}", hits[0].1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn time_travel_shows_the_field_growing() {
        let (mut engine, dir) = temp_engine();
        let ing = ingest_papers(&mut engine, &synthetic_papers());
        let early = count_papers_at(&engine, ing.timeline.first().unwrap().1);
        let late = count_papers_at(&engine, ing.timeline.last().unwrap().1);
        assert!(early < late, "as_of earliest year ({early}) sees fewer papers than latest ({late})");
        assert_eq!(late, ing.papers, "latest snapshot sees every paper");
        let _ = std::fs::remove_dir_all(dir);
    }
}
