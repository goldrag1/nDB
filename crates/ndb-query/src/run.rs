//! Text → result fusion.
//!
//! The crate's other modules expose each step of the pipeline
//! individually (lex → parse → resolve → execute). Callers that want a
//! one-call surface ("give me the rows for this query string") go
//! through this module instead.
//!
//! ```rust,no_run
//! use ndb_engine::Engine;
//! use ndb_query::execute_text;
//! # let mut engine: Engine = unimplemented!();
//! let resp = execute_text(&mut engine, "match species() as ?s return ?s limit 5").unwrap();
//! assert!(resp.rows.len() <= 5);
//! ```
//!
//! The runner builds a fresh [`Dictionaries`] snapshot on every call by
//! walking the engine's records. That's O(records) per query, which is
//! acceptable for v1 — dictionary entries (`TypeName`, `RoleName`,
//! `PropertyKey`) are rare in the snapshot and won't dominate. Caching
//! the dictionaries on the engine across calls is a v2 follow-up.

use ndb_engine::query::{QueryError, execute};
use ndb_engine::{Engine, EngineError, QueryRequest, QueryResponse, TxId};
use serde::Serialize;

use crate::error::ParseError;
use crate::parse::parse_query;
use crate::resolve::{Dictionaries, ResolveError, resolve};

/// Aggregate error type returned by the text-→-result entry points.
/// One variant per stage of the pipeline; downstream consumers (server
/// route, CLI, client) match on `code()` to map to HTTP status / exit
/// code.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// Lexer or parser rejected the surface text.
    #[error("{0}")]
    Parse(#[from] ParseError),
    /// Name resolution failed against the engine's dictionaries
    /// (unknown type / role / property; unbound variable; etc.).
    #[error("{0}")]
    Resolve(#[from] ResolveError),
    /// The engine refused to execute the resolved request (e.g. `as_of`
    /// pointed at a tx the engine doesn't know about).
    #[error("{0}")]
    Query(#[from] QueryError),
    /// Snapshot iteration failed while we were building the dictionary
    /// (storage I/O error).
    #[error("{0}")]
    Engine(#[from] EngineError),
}

impl RunError {
    /// Short error code for HTTP / CLI status mapping. Mirrors the codes
    /// used in the working spec's §6 error model.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(e)   => e.code(),
            Self::Resolve(e) => e.code(),
            Self::Query(_)   => "query_error",
            Self::Engine(_)  => "engine_error",
        }
    }
}

/// JSON-friendly error envelope. Used by the server route and the CLI.
#[derive(Debug, Serialize)]
pub struct RunErrorEnvelope<'a> {
    /// Short error class — `parse`, `resolve`, `query`, or `engine`.
    pub error: &'a str,
    /// Specific code from the inner error (`unknown_type`, `unexpected_token`, …).
    pub code: &'a str,
    /// Human-readable detail.
    pub detail: String,
    /// Source span, if the error has one (parse / resolve errors).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SpanInfo>,
}

/// Span info reduced to a serializable shape.
#[derive(Debug, Serialize)]
pub struct SpanInfo {
    /// Byte offset of the span start in the source text.
    pub start: usize,
    /// Byte offset of the span end (exclusive).
    pub end: usize,
}

impl RunError {
    /// Construct a JSON-friendly envelope, suitable for the server's
    /// 4xx response body and the CLI's stderr serialisation.
    #[must_use]
    pub fn envelope(&self) -> RunErrorEnvelope<'_> {
        let (class, span) = match self {
            Self::Parse(e)   => ("parse",   Some(e.span())),
            Self::Resolve(e) => ("resolve", Some(e.span())),
            Self::Query(_)   => ("query",   None),
            Self::Engine(_)  => ("engine",  None),
        };
        RunErrorEnvelope {
            error: class,
            code: self.code(),
            detail: self.to_string(),
            span: span.map(|s| SpanInfo { start: s.start, end: s.end() }),
        }
    }
}

/// Parse + resolve text to a wire-AST [`QueryRequest`], using the
/// engine's current dictionary snapshot. Does NOT execute.
///
/// Callers who want to inspect the plan, send the AST elsewhere, or
/// cache the resolved request use this entry point.
pub fn parse_resolve(engine: &Engine, text: &str) -> Result<QueryRequest, RunError> {
    let name_query = parse_query(text)?;
    let dict = build_dictionary(engine)?;
    Ok(resolve(name_query, &dict)?)
}

/// Lex → parse → resolve → execute. The full end-to-end path.
pub fn execute_text(engine: &mut Engine, text: &str) -> Result<QueryResponse, RunError> {
    let req = parse_resolve(engine, text)?;
    Ok(execute(engine, req)?)
}

/// Read-only variant of [`execute_text`] — takes `&Engine`, executes via
/// `query::execute_read`. Errors if the resolved request carries any
/// write clauses. Callers that hold a `RwLock<Engine>` read guard use
/// this so concurrent readers parallelise.
pub fn execute_text_read(engine: &Engine, text: &str) -> Result<QueryResponse, RunError> {
    let req = parse_resolve(engine, text)?;
    Ok(ndb_engine::query::execute_read(engine, req)?)
}

/// Build a name-resolution dictionary from the engine's current
/// snapshot.
fn build_dictionary(engine: &Engine) -> Result<Dictionaries, RunError> {
    // TxId::ACTIVE is "latest committed" — what /iter and the executor
    // both use as the implicit snapshot when no `as_of` is set.
    let mut dict = Dictionaries::default();
    for r in engine.snapshot_iter_streaming(TxId::ACTIVE) {
        let r = r?;
        dict_observe(&mut dict, &r);
    }
    Ok(dict)
}

fn dict_observe(dict: &mut Dictionaries, r: &ndb_engine::record::Record) {
    use crate::resolve::TypeKindObserved;
    use ndb_engine::record::Record;
    match r {
        Record::TypeName(t)    => { dict.types.insert(t.name.clone(), t.id.get()); }
        Record::RoleName(r)    => { dict.roles.insert(r.name.clone(), r.id.get()); }
        Record::PropertyKey(p) => { dict.properties.insert(p.name.clone(), p.id.get()); }
        Record::Entity(e) => {
            let prev = dict.type_kinds.get(&e.type_id.get()).copied();
            let merged = match prev {
                Some(TypeKindObserved::Hyperedge) | Some(TypeKindObserved::Both) => TypeKindObserved::Both,
                _ => TypeKindObserved::Entity,
            };
            dict.type_kinds.insert(e.type_id.get(), merged);
        }
        Record::HyperEdge(h) => {
            let prev = dict.type_kinds.get(&h.type_id.get()).copied();
            let merged = match prev {
                Some(TypeKindObserved::Entity) | Some(TypeKindObserved::Both) => TypeKindObserved::Both,
                _ => TypeKindObserved::Hyperedge,
            };
            dict.type_kinds.insert(h.type_id.get(), merged);
        }
        _ => {}
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ndb_engine::Engine;
    use ndb_engine::id::{EntityId, PropertyId, RoleId, TxId, TypeId};
    use ndb_engine::record::{EntityRecord, HyperEdgeRecord, PropertyKeyRecord, Record, RoleNameRecord, TypeNameRecord};
    use ndb_engine::value::Value;

    /// Construct a tiny engine: one Customer entity type, one Sales hyperedge
    /// type with seller/buyer roles, two entities + one hyperedge linking them.
    fn build_engine() -> (Engine, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ndb-query-run-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut engine = Engine::create(&dir).expect("create");
        // Register names via put_raw (dictionary records carry no tx_id fields).
        let mut tx = engine.begin_write();
        tx.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(1),   name: "customer".into() }));
        tx.put_raw(Record::TypeName(TypeNameRecord { id: TypeId::new(100), name: "purchase".into() }));
        tx.put_raw(Record::RoleName(RoleNameRecord { id: RoleId::new(10),  name: "buyer".into() }));
        tx.put_raw(Record::RoleName(RoleNameRecord { id: RoleId::new(11),  name: "seller".into() }));
        tx.put_raw(Record::PropertyKey(PropertyKeyRecord { id: PropertyId::new(30), name: "name".into() }));
        tx.commit().expect("commit names");

        // Two customers + one purchase hyperedge
        let alice = EntityId::now_v7();
        let bob   = EntityId::now_v7();
        let mut tx = engine.begin_write();
        let txid = tx.tx_id();
        tx.put_entity(EntityRecord { entity_id: alice, type_id: TypeId::new(1),
            tx_id_assert: txid, tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(30), Value::String("Alice".into()))] });
        tx.put_entity(EntityRecord { entity_id: bob, type_id: TypeId::new(1),
            tx_id_assert: txid, tx_id_supersede: TxId::ACTIVE,
            properties: vec![(PropertyId::new(30), Value::String("Bob".into()))] });
        tx.commit().expect("commit entities");

        let mut tx = engine.begin_write();
        let txid = tx.tx_id();
        tx.put_hyperedge(HyperEdgeRecord {
            hyperedge_id: ndb_engine::id::HyperedgeId::now_v7(),
            type_id: TypeId::new(100),
            tx_id_assert: txid, tx_id_supersede: TxId::ACTIVE,
            roles: vec![(RoleId::new(10), bob), (RoleId::new(11), alice)],
            hyperedge_roles: Vec::new(),
            properties: vec![],
        });
        tx.commit().expect("commit edge");

        (engine, dir)
    }


    #[test]
    fn execute_text_returns_entities_by_type() {
        let (mut engine, _dir) = build_engine();
        let resp = execute_text(&mut engine, "match customer() as ?c return ?c").expect("query");
        assert_eq!(resp.columns, vec!["c"]);
        assert_eq!(resp.rows.len(), 2, "expected both customers, got {:?}", resp.rows);
    }

    #[test]
    fn execute_text_filters_by_property() {
        let (mut engine, _dir) = build_engine();
        let resp = execute_text(&mut engine,
            "match customer(name: \"Alice\") as ?c return ?c"
        ).expect("query");
        assert_eq!(resp.rows.len(), 1);
    }

    #[test]
    fn execute_text_returns_hyperedge_role_filler() {
        let (mut engine, _dir) = build_engine();
        let resp = execute_text(&mut engine,
            "match purchase(buyer: ?b, seller: ?s) return ?b, ?s"
        ).expect("query");
        assert_eq!(resp.columns, vec!["b", "s"]);
        assert_eq!(resp.rows.len(), 1);
        assert_eq!(resp.rows[0].len(), 2);
    }

    #[test]
    fn parse_error_surfaces_with_span() {
        let (mut engine, _dir) = build_engine();
        let err = execute_text(&mut engine, "this is not a query").unwrap_err();
        let env = err.envelope();
        assert_eq!(env.error, "parse");
        assert!(env.span.is_some(), "parse errors must carry a span");
    }

    #[test]
    fn unknown_type_surfaces_as_resolve_error() {
        let (mut engine, _dir) = build_engine();
        let err = execute_text(&mut engine, "match planet() as ?p return ?p").unwrap_err();
        let env = err.envelope();
        assert_eq!(env.error, "resolve");
        assert_eq!(env.code, "unknown_type");
    }

    #[test]
    fn create_entity_then_match_then_delete_full_lifecycle() {
        let (mut engine, _dir) = build_engine();

        // Sanity: 2 customers before any writes.
        let before = execute_text(&mut engine, "match customer() as ?c return ?c").expect("match");
        assert_eq!(before.rows.len(), 2);

        // CREATE — add Charlie + project his properties back.
        let created = execute_text(&mut engine,
            r#"create customer(name: "Charlie") as ?new return ?new.name"#
        ).expect("create");
        assert_eq!(created.rows.len(), 1);
        let charlie_name = match &created.rows[0][0] {
            ndb_engine::JsonValue::String { value } => value.clone(),
            other => panic!("expected string, got {other:?}"),
        };
        assert_eq!(charlie_name, "Charlie");

        // MATCH — confirm Charlie is now findable.
        let after_create = execute_text(&mut engine, "match customer() as ?c return ?c.name").expect("match");
        assert_eq!(after_create.rows.len(), 3);

        // DELETE — tombstone Charlie.
        let deleted = execute_text(&mut engine,
            r#"match customer(name: "Charlie") as ?c delete ?c return ?c.name"#
        ).expect("delete");
        assert_eq!(deleted.rows.len(), 1, "delete should return what was tombstoned");

        // MATCH — Charlie is gone.
        let after_delete = execute_text(&mut engine, "match customer() as ?c return ?c").expect("match");
        assert_eq!(after_delete.rows.len(), 2);
    }

    #[test]
    fn create_hyperedge_with_bound_role_fillers() {
        let (mut engine, _dir) = build_engine();
        // The build_engine fixture has Alice + Bob + one buyer/seller purchase.
        // Create a SECOND purchase via the query language, naming the same
        // entities by bound variables.
        let resp = execute_text(&mut engine,
            r#"match customer(name: "Alice") as ?a
                     customer(name: "Bob")   as ?b
               create purchase(buyer: ?a, seller: ?b) as ?p2
               return ?p2"#
        ).expect("create hyperedge");
        assert_eq!(resp.rows.len(), 1);
        // Verify by counting purchases now.
        let count = execute_text(&mut engine, "match purchase() as ?p return ?p").expect("count");
        assert_eq!(count.rows.len(), 2, "should be two purchase hyperedges now");
    }

    #[test]
    fn query_with_no_match_or_create_errors() {
        let (mut engine, _dir) = build_engine();
        let err = execute_text(&mut engine, "return ?c").unwrap_err();
        assert_eq!(err.envelope().error, "parse");
    }

    #[test]
    fn execute_text_order_by_property_ascending_then_descending() {
        let (mut engine, _dir) = build_engine();
        let asc = execute_text(&mut engine,
            "match customer(name: ?n) as ?c return ?c.name order by ?c.name asc"
        ).expect("query");
        let asc_names: Vec<String> = asc.rows.iter()
            .filter_map(|r| match &r[0] {
                ndb_engine::JsonValue::String { value } => Some(value.clone()),
                _ => None,
            }).collect();
        assert_eq!(asc_names, vec!["Alice", "Bob"]);

        let desc = execute_text(&mut engine,
            "match customer(name: ?n) as ?c return ?c.name order by ?c.name desc"
        ).expect("query");
        let desc_names: Vec<String> = desc.rows.iter()
            .filter_map(|r| match &r[0] {
                ndb_engine::JsonValue::String { value } => Some(value.clone()),
                _ => None,
            }).collect();
        assert_eq!(desc_names, vec!["Bob", "Alice"]);
    }

    #[test]
    fn execute_text_property_projection_returns_scalars() {
        let (mut engine, _dir) = build_engine();
        // ?c.name should project the literal "Alice" / "Bob" string,
        // not the entity UUID.
        let resp = execute_text(&mut engine,
            "match customer(name: ?n) as ?c return ?c.name, ?n"
        ).expect("query");
        assert_eq!(resp.columns, vec!["c.name", "n"]);
        assert_eq!(resp.rows.len(), 2);
        // Each row should contain the customer's name string in both columns.
        for row in &resp.rows {
            assert_eq!(row[0], row[1], "?c.name and ?n should both bind to the name string");
        }
    }

    #[test]
    fn property_projection_unknown_property_surfaces_resolve_error() {
        let (mut engine, _dir) = build_engine();
        let err = execute_text(&mut engine,
            "match customer() as ?c return ?c.nonexistent"
        ).unwrap_err();
        let env = err.envelope();
        assert_eq!(env.error, "resolve");
        assert_eq!(env.code, "unknown_role_or_property");
    }

    #[test]
    fn parse_resolve_returns_request_without_executing() {
        let (engine, _dir) = build_engine();
        let req = parse_resolve(&engine, "match customer() as ?c return ?c").expect("parse_resolve");
        assert_eq!(req.patterns.len(), 1);
        assert_eq!(req.returns, vec![ndb_engine::ReturnItem::from("c")]);
    }
}
