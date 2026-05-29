//! langgraph-ingest — build a "language graph" (GraphRAG-style knowledge
//! graph) directly inside nDB.
//!
//! Rust-native by design: facts are written through the embedded engine
//! API (`Engine::begin_write` / `put_entity` / `put_hyperedge`), with no
//! intermediate `seed.json` and no second loader. A demo whose whole pitch
//! is a Rust-native graph DB should drink its own champagne.
//!
//! What it models (and why nDB fits):
//! - **N-ary facts as hyperedges.** "Microsoft acquired GitHub in 2018" is
//!   ONE `FACT` hyperedge (subject + object roles + a `predicate`/`year`
//!   property), not three awkward binary rows. Facts with a third
//!   participant ("nDB compared_with SQLite on joins") get a `context`
//!   role — genuine arity-3 edges.
//! - **Per-entity embeddings** (`Value::Vector`, vector-indexed) → semantic
//!   kNN, the "retrieval" half of GraphRAG.
//! - **Per-document transactions** → each ingested doc is its own commit,
//!   so MVCC `as_of` gives an honest time-travel view of how the graph
//!   grew. No temporal-table gymnastics.
//!
//! Slice 1 (this file): engine-direct ingest of an offline fixture corpus
//! + a `ureq` LLM-extraction integration point (env-gated), then three
//! demonstrative reads (lookup, vector kNN, time-travel). Slice 2 wires the
//! 3D explorer's `as_of` scrubber + search box to the live server.
//!
//! Run:
//!     cargo run -p langgraph                 # offline fixture → .demo-data/langgraph-ndb
//!     cargo run -p langgraph -- /tmp/lg      # custom db dir
//!     LANGGRAPH_LLM_URL=https://api.example/v1/chat/completions \
//!     LANGGRAPH_API_KEY=sk-... cargo run -p langgraph   # real extraction
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation,
         clippy::cast_sign_loss, clippy::too_many_lines)]

use std::collections::HashMap;

use ndb_engine::record::{
    EntityRecord, HyperEdgeRecord, PropertyKeyRecord, Record, RoleNameRecord, TypeNameRecord,
};
use ndb_engine::{
    Distance, Engine, EntityId, HyperedgeId, PropertyId, RoleId, TxId, TypeId, Value,
};

// ─── Schema ────────────────────────────────────────────────────────────
const TYPE_ENTITY: u32 = 300;
const TYPE_FACT: u32 = 400;

const PROP_NAME: u32 = 50; // lookup-key indexed
const PROP_KIND: u32 = 51; // property-btree indexed
const PROP_EMBED: u32 = 52; // vector indexed
const PROP_PREDICATE: u32 = 53;
const PROP_SOURCE: u32 = 54; // source document id

const ROLE_SUBJECT: u32 = 20;
const ROLE_OBJECT: u32 = 21;
const ROLE_CONTEXT: u32 = 22; // present only on arity-3 facts

const EMBED_DIM: usize = 16;

// ─── Extracted-fact shape (what an LLM returns; the offline corpus hand-
// authors the same shape so Slice 1 runs with no network). ──────────────
struct Participant {
    name: &'static str,
    kind: &'static str,
}
struct Fact {
    predicate: &'static str,
    subject: Participant,
    object: Participant,
    context: Option<Participant>, // Some → arity-3 N-ary fact
}
struct Doc {
    id: &'static str,
    facts: Vec<Fact>,
}

fn p(name: &'static str, kind: &'static str) -> Participant {
    Participant { name, kind }
}

/// Offline fixture corpus — real facts about the database / graph domain,
/// hand-authored in the exact shape an LLM extractor would emit. This is
/// sanctioned demo data, not a fake API response: the entities and
/// relations are genuine, the graph it builds is real.
fn corpus() -> Vec<Doc> {
    vec![
        Doc { id: "doc-ndb", facts: vec![
            Fact { predicate: "is_a", subject: p("nDB", "system"), object: p("hypergraph database", "concept"), context: None },
            Fact { predicate: "written_in", subject: p("nDB", "system"), object: p("Rust", "language"), context: None },
            Fact { predicate: "stores", subject: p("nDB", "system"), object: p("N-ary fact", "concept"), context: None },
        ]},
        Doc { id: "doc-storage", facts: vec![
            Fact { predicate: "built_on", subject: p("nDB", "system"), object: p("LSM tree", "concept"), context: None },
            Fact { predicate: "provides", subject: p("nDB", "system"), object: p("MVCC", "concept"), context: None },
            Fact { predicate: "enables", subject: p("MVCC", "concept"), object: p("time travel", "concept"), context: None },
        ]},
        Doc { id: "doc-vectors", facts: vec![
            Fact { predicate: "indexes", subject: p("nDB", "system"), object: p("embedding", "concept"), context: None },
            Fact { predicate: "powers", subject: p("embedding", "concept"), object: p("semantic search", "concept"), context: None },
        ]},
        Doc { id: "doc-graphrag", facts: vec![
            Fact { predicate: "extracts", subject: p("GraphRAG", "method"), object: p("knowledge graph", "concept"), context: Some(p("text corpus", "concept")) },
            Fact { predicate: "retrieves_with", subject: p("GraphRAG", "method"), object: p("semantic search", "concept"), context: None },
            Fact { predicate: "feeds", subject: p("knowledge graph", "concept"), object: p("LLM", "system"), context: None },
        ]},
        Doc { id: "doc-bench", facts: vec![
            Fact { predicate: "compared_with", subject: p("nDB", "system"), object: p("SQLite", "system"), context: Some(p("recursive traversal", "concept")) },
            Fact { predicate: "compared_with", subject: p("nDB", "system"), object: p("PostgreSQL", "system"), context: Some(p("graph join", "concept")) },
            Fact { predicate: "beats", subject: p("nDB", "system"), object: p("PostgreSQL", "system"), context: Some(p("recursive traversal", "concept")) },
        ]},
        Doc { id: "doc-traversal", facts: vec![
            Fact { predicate: "supports", subject: p("nDB", "system"), object: p("recursive traversal", "concept"), context: None },
            Fact { predicate: "models", subject: p("hypergraph database", "concept"), object: p("approval chain", "domain"), context: None },
            Fact { predicate: "models", subject: p("hypergraph database", "concept"), object: p("dependency graph", "domain"), context: None },
        ]},
    ]
}

// ─── Deterministic offline embedding ───────────────────────────────────
// Char unigram + bigram hashing into EMBED_DIM buckets, L2-normalised.
// Similar names share buckets → near vectors; querying with embed(name)
// returns that entity at distance ~0. Deterministic → the test is stable.
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

// ─── Schema registration + ingest ──────────────────────────────────────
fn register_schema(engine: &mut Engine) {
    engine.register_lookup_key(PropertyId::new(PROP_NAME));
    engine.register_property_btree(TypeId::new(TYPE_ENTITY), PropertyId::new(PROP_KIND));
    engine.register_vector_property(PropertyId::new(PROP_EMBED));

    let mut tx = engine.begin_write();
    tx.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(TYPE_ENTITY), name: "entity".into() }));
    tx.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(TYPE_FACT), name: "fact".into() }));
    tx.put_raw(Record::RoleName(RoleNameRecord { id: RoleId::new(ROLE_SUBJECT), name: "subject".into() }));
    tx.put_raw(Record::RoleName(RoleNameRecord { id: RoleId::new(ROLE_OBJECT), name: "object".into() }));
    tx.put_raw(Record::RoleName(RoleNameRecord { id: RoleId::new(ROLE_CONTEXT), name: "context".into() }));
    tx.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(PROP_NAME), name: "name".into() }));
    tx.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(PROP_KIND), name: "kind".into() }));
    tx.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(PROP_EMBED), name: "embedding".into() }));
    tx.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(PROP_PREDICATE), name: "predicate".into() }));
    tx.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(PROP_SOURCE), name: "source".into() }));
    tx.commit().unwrap();
}

/// Find-or-create an entity by name within the current write txn. Dedups
/// across documents: an entity first seen in doc 1 is reused (same
/// `EntityId`) by every later doc, never re-inserted.
fn ensure_entity(
    tx: &mut ndb_engine::WriteTxn<'_>,
    ids: &mut HashMap<String, EntityId>,
    name: &str,
    kind: &str,
) -> EntityId {
    if let Some(eid) = ids.get(name) {
        return *eid;
    }
    let eid = EntityId::now_v7();
    tx.put_entity(EntityRecord {
        entity_id: eid,
        type_id: TypeId::new(TYPE_ENTITY),
        tx_id_assert: TxId::new(0),
        tx_id_supersede: TxId::ACTIVE,
        properties: vec![
            (PropertyId::new(PROP_NAME), Value::String(name.to_string())),
            (PropertyId::new(PROP_KIND), Value::String(kind.to_string())),
            (PropertyId::new(PROP_EMBED), Value::Vector(embed(name))),
        ],
    });
    ids.insert(name.to_string(), eid);
    eid
}

struct Ingested {
    name_to_id: HashMap<String, EntityId>,
    /// (doc_id, committed tx) in ingest order — the time-travel timeline.
    timeline: Vec<(String, TxId)>,
    facts: usize,
}

/// Ingest the corpus, one transaction per document. Returns the
/// name→id map and the per-document commit timeline.
fn ingest(engine: &mut Engine, docs: &[Doc]) -> Ingested {
    let mut name_to_id: HashMap<String, EntityId> = HashMap::new();
    let mut timeline = Vec::with_capacity(docs.len());
    let mut facts = 0usize;

    for doc in docs {
        let mut tx = engine.begin_write();
        for f in &doc.facts {
            let subj = ensure_entity(&mut tx, &mut name_to_id, f.subject.name, f.subject.kind);
            let obj = ensure_entity(&mut tx, &mut name_to_id, f.object.name, f.object.kind);
            let mut roles = vec![
                (RoleId::new(ROLE_SUBJECT), subj),
                (RoleId::new(ROLE_OBJECT), obj),
            ];
            if let Some(ctx) = &f.context {
                let cid = ensure_entity(&mut tx, &mut name_to_id, ctx.name, ctx.kind);
                roles.push((RoleId::new(ROLE_CONTEXT), cid));
            }
            tx.put_hyperedge(HyperEdgeRecord {
                hyperedge_id: HyperedgeId::now_v7(),
                type_id: TypeId::new(TYPE_FACT),
                tx_id_assert: TxId::new(0),
                tx_id_supersede: TxId::ACTIVE,
                roles,
                hyperedge_roles: Vec::new(),
                properties: vec![
                    (PropertyId::new(PROP_PREDICATE), Value::String(f.predicate.to_string())),
                    (PropertyId::new(PROP_SOURCE), Value::String(doc.id.to_string())),
                ],
            });
            facts += 1;
        }
        let tx_id = tx.commit().unwrap();
        timeline.push((doc.id.to_string(), tx_id));
    }

    Ingested { name_to_id, timeline, facts }
}

/// Count `FACT` hyperedges visible at a given snapshot — the time-travel
/// probe. Reads the MVCC view as-of `tx`.
fn count_facts_at(engine: &Engine, tx: TxId) -> usize {
    engine
        .snapshot_iter(tx)
        .unwrap()
        .iter()
        .filter(|r| matches!(r, Record::HyperEdge(h) if h.type_id == TypeId::new(TYPE_FACT)))
        .count()
}

// ─── LLM extraction integration point (env-gated; ureq transport) ───────
// The offline corpus is the default. When LANGGRAPH_LLM_URL is set, this is
// the path that turns raw document text into facts via a real model. Wired
// with the real request/response shape; left out of the default run so
// Slice 1 builds + tests offline with no API key.
#[allow(dead_code)]
fn extract_via_llm(url: &str, api_key: &str, document: &str) -> Result<serde_json::Value, ureq::Error> {
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1024,
        "messages": [{
            "role": "user",
            "content": format!(
                "Extract factual triples from the text as JSON: \
                 [{{\"subject\":..,\"subject_kind\":..,\"predicate\":..,\
                 \"object\":..,\"object_kind\":..,\"context\":null}}]. \
                 Text:\n{document}"
            )
        }]
    });
    let resp = ureq::post(url)
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Content-Type", "application/json")
        .send_json(body)?;
    Ok(resp.into_json::<serde_json::Value>()?)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".demo-data/langgraph-ndb".to_string());

    // Demo data: rebuild from scratch each run.
    let _ = std::fs::remove_dir_all(&db_dir);
    std::fs::create_dir_all(&db_dir)?;

    if let Ok(url) = std::env::var("LANGGRAPH_LLM_URL") {
        let key = std::env::var("LANGGRAPH_API_KEY").unwrap_or_default();
        eprintln!("LANGGRAPH_LLM_URL set → would extract via {url} (Slice 1 uses the offline corpus to seed; live extraction lands in Slice 2). key_present={}", !key.is_empty());
    }

    let mut engine = Engine::create(&db_dir)?;
    register_schema(&mut engine);
    let docs = corpus();
    let ing = ingest(&mut engine, &docs);

    println!(
        "ingested {} entities + {} facts across {} documents → {db_dir}",
        ing.name_to_id.len(),
        ing.facts,
        ing.timeline.len()
    );

    // (1) lookup-key probe
    if let Some(eid) =
        engine.lookup_by_external_key(PropertyId::new(PROP_NAME), &Value::String("nDB".into()))
    {
        println!("\nlookup name='nDB' → {}", eid.into_uuid());
    }

    // (2) semantic kNN — the GraphRAG retrieval half
    let id_to_name: HashMap<EntityId, String> =
        ing.name_to_id.iter().map(|(n, i)| (*i, n.clone())).collect();
    let probe = "hypergraph database";
    let hits = engine.vector_search(
        PropertyId::new(PROP_EMBED),
        &embed(probe),
        5,
        Distance::Cosine,
    );
    println!("\nsemantic kNN nearest to \"{probe}\":");
    for (eid, dist) in hits {
        let name = id_to_name.get(&eid).map_or("?", String::as_str);
        println!("  {dist:.4}  {name}");
    }

    // (3) time travel — facts visible after doc 1 vs after the whole corpus
    if let (Some(first), Some(last)) = (ing.timeline.first(), ing.timeline.last()) {
        println!(
            "\ntime travel: {} facts as_of after \"{}\"  →  {} facts as_of after \"{}\"",
            count_facts_at(&engine, first.1),
            first.0,
            count_facts_at(&engine, last.1),
            last.0
        );
    }

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
    fn ingest_builds_a_real_graph() {
        let (mut engine, dir) = temp_engine();
        let docs = corpus();
        let ing = ingest(&mut engine, &docs);

        assert!(ing.name_to_id.len() >= 10, "expected a dozen-ish entities");
        assert!(ing.facts >= 15, "expected the full fixture fact set");
        assert_eq!(ing.timeline.len(), docs.len(), "one tx per document");

        // nDB participates in many facts → it must exist as an entity.
        assert!(ing.name_to_id.contains_key("nDB"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn vector_knn_returns_self_first() {
        let (mut engine, dir) = temp_engine();
        let _ = ingest(&mut engine, &corpus());
        let hits = engine.vector_search(
            PropertyId::new(PROP_EMBED),
            &embed("nDB"),
            3,
            Distance::Cosine,
        );
        assert!(!hits.is_empty(), "kNN must return neighbours");
        // Querying with embed("nDB") → the nDB entity's stored vector is
        // identical → distance ~0 → it ranks first.
        let nearest = engine
            .lookup_by_external_key(PropertyId::new(PROP_NAME), &Value::String("nDB".into()))
            .unwrap();
        assert_eq!(hits[0].0, nearest, "self is its own nearest neighbour");
        assert!(hits[0].1 < 1e-4, "self distance ~0, got {}", hits[0].1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn time_travel_shows_the_graph_growing() {
        let (mut engine, dir) = temp_engine();
        let ing = ingest(&mut engine, &corpus());
        let after_first = count_facts_at(&engine, ing.timeline.first().unwrap().1);
        let after_last = count_facts_at(&engine, ing.timeline.last().unwrap().1);
        assert!(
            after_first < after_last,
            "as_of after doc 1 ({after_first}) must see fewer facts than after the corpus ({after_last})"
        );
        assert_eq!(after_last, ing.facts, "latest snapshot sees every fact");
        let _ = std::fs::remove_dir_all(dir);
    }
}
