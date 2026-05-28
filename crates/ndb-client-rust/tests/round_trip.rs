//! End-to-end round-trip tests for the Rust client against a real
//! `ndb-server` (in-process via the `ndb-server` crate).
#![allow(clippy::doc_markdown)]

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ndb_client::{Client, ClientError};
use ndb_engine::{
    CommitRequest, EntityId, JsonProperty, JsonRecord, JsonValue, PropertyId, TxIdOrActive,
    TypeId, Value,
};
use ndb_server::Server;

fn temp_dir(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ndb-client-rust-{}-{}",
        name,
        uuid::Uuid::now_v7().simple()
    ));
    p
}

fn spawn(server: &Arc<Server>, n_conn: usize) -> std::net::SocketAddr {
    let bind = server.bind("127.0.0.1:0").unwrap();
    let addr = bind.local_addr().unwrap();
    drop(bind);
    let srv = Arc::clone(server);
    thread::spawn(move || {
        let bind = srv.bind(addr).unwrap();
        let _ = bind.serve_n(n_conn);
    });
    thread::sleep(Duration::from_millis(50));
    addr
}

fn client(addr: std::net::SocketAddr) -> Client {
    Client::new(&format!("http://{addr}")).unwrap()
}

#[test]
fn health_round_trip() {
    let dir = temp_dir("health");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn(&server, 1);
    let resp = client(addr).health().unwrap();
    assert_eq!(resp.status, "ok");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn commit_then_read_round_trip() {
    let dir = temp_dir("commit_read");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn(&server, 2);
    let cli = client(addr);

    let alice = EntityId::now_v7();
    let req = CommitRequest {
        records: vec![JsonRecord::Entity {
            entity_id: alice.into_uuid().to_string(),
            type_id: 1,
            tx_id_assert: 0,
            tx_id_supersede: TxIdOrActive::Active,
            properties: vec![JsonProperty {
                prop_id: 10,
                value: JsonValue::String {
                    value: "alice@example.com".to_owned(),
                },
            }],
        }],
    };
    let commit = cli.commit(&req).unwrap();
    assert!(commit.tx_id > 0);

    let read = cli.read(&alice.into_uuid().to_string()).unwrap();
    match read {
        ndb_engine::ReadResponse::Live { record } => match record {
            JsonRecord::Entity { entity_id, .. } => {
                assert_eq!(entity_id, alice.into_uuid().to_string());
            }
            other => panic!("expected entity, got {other:?}"),
        },
        other => panic!("expected Live, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn iter_returns_committed_records() {
    let dir = temp_dir("iter");
    let server = Arc::new(Server::open(&dir).unwrap());
    {
        let e = server.engine();
        let mut e = e.lock().unwrap();
        for i in 0..3 {
            let mut txn = e.begin_write();
            txn.put_entity(ndb_engine::EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: TypeId::new(1),
                tx_id_assert: ndb_engine::TxId::new(0),
                tx_id_supersede: ndb_engine::TxId::ACTIVE,
                properties: vec![(PropertyId::new(1), Value::I64(i))],
            });
            txn.commit().unwrap();
        }
    }
    let addr = spawn(&server, 1);
    let records = client(addr).iter().unwrap();
    assert_eq!(records.len(), 3);
    for r in records {
        match r {
            JsonRecord::Entity { .. } => {}
            other => panic!("expected entity, got {other:?}"),
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn lookup_by_key_via_indexed_route() {
    let dir = temp_dir("lookup");
    let server = Arc::new(Server::open(&dir).unwrap());

    let alice = EntityId::now_v7();
    {
        let e = server.engine();
        let mut e = e.lock().unwrap();
        e.register_lookup_key(PropertyId::new(10));
        let mut txn = e.begin_write();
        txn.put_entity(ndb_engine::EntityRecord {
            entity_id: alice,
            type_id: TypeId::new(1),
            tx_id_assert: ndb_engine::TxId::new(0),
            tx_id_supersede: ndb_engine::TxId::ACTIVE,
            properties: vec![(
                PropertyId::new(10),
                Value::String("alice@example.com".into()),
            )],
        });
        txn.commit().unwrap();
    }
    let addr = spawn(&server, 2);
    let cli = client(addr);

    let hit = cli
        .lookup_by_key(
            10,
            JsonValue::String {
                value: "alice@example.com".into(),
            },
        )
        .unwrap();
    assert_eq!(hit, Some(alice.into_uuid().to_string()));

    let miss = cli
        .lookup_by_key(
            10,
            JsonValue::String {
                value: "nobody@example.com".into(),
            },
        )
        .unwrap();
    assert!(miss.is_none());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn vector_search_route_returns_sorted_hits() {
    let dir = temp_dir("vec");
    let server = Arc::new(Server::open(&dir).unwrap());

    let a = EntityId::now_v7();
    let b = EntityId::now_v7();
    {
        let e = server.engine();
        let mut e = e.lock().unwrap();
        e.register_vector_property(PropertyId::new(20));
        for (eid, v) in [(a, vec![1.0_f32, 0.0]), (b, vec![0.0_f32, 1.0])] {
            let mut txn = e.begin_write();
            txn.put_entity(ndb_engine::EntityRecord {
                entity_id: eid,
                type_id: TypeId::new(1),
                tx_id_assert: ndb_engine::TxId::new(0),
                tx_id_supersede: ndb_engine::TxId::ACTIVE,
                properties: vec![(PropertyId::new(20), Value::Vector(v))],
            });
            txn.commit().unwrap();
        }
    }
    let addr = spawn(&server, 2);
    let cli = client(addr);

    let hits = cli
        .vector_search(20, &[1.0, 0.0], 2, ndb_engine::VectorMetric::L2)
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].entity_id, a.into_uuid().to_string());

    // k=0 → 400.
    let err = cli
        .vector_search(20, &[1.0, 0.0], 0, ndb_engine::VectorMetric::L2)
        .unwrap_err();
    match err {
        ClientError::Http { status, .. } => assert_eq!(status, 400),
        other => panic!("expected Http 400, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn property_lookup_and_range_round_trip() {
    let dir = temp_dir("prop");
    let server = Arc::new(Server::open(&dir).unwrap());

    let alice = EntityId::now_v7();
    let bob = EntityId::now_v7();
    {
        let e = server.engine();
        let mut e = e.lock().unwrap();
        e.register_property_btree(TypeId::new(1), PropertyId::new(30));
        for (eid, age) in [(alice, 25_i64), (bob, 35)] {
            let mut txn = e.begin_write();
            txn.put_entity(ndb_engine::EntityRecord {
                entity_id: eid,
                type_id: TypeId::new(1),
                tx_id_assert: ndb_engine::TxId::new(0),
                tx_id_supersede: ndb_engine::TxId::ACTIVE,
                properties: vec![(PropertyId::new(30), Value::I64(age))],
            });
            txn.commit().unwrap();
        }
    }
    let addr = spawn(&server, 3);
    let cli = client(addr);

    // Exact match.
    let hits = cli
        .property_lookup(1, 30, JsonValue::I64 { value: 35 })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0], bob.into_uuid().to_string());

    // Range — inclusive both ends.
    let hits = cli
        .property_range(
            1,
            30,
            Some(JsonValue::I64 { value: 25 }),
            Some(JsonValue::I64 { value: 30 }),
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0], alice.into_uuid().to_string());

    // Unbounded high.
    let hits = cli
        .property_range(1, 30, Some(JsonValue::I64 { value: 30 }), None)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0], bob.into_uuid().to_string());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn flush_and_compact_round_trip() {
    let dir = temp_dir("flush_compact");
    let server = Arc::new(Server::open(&dir).unwrap());
    {
        let e = server.engine();
        let mut e = e.lock().unwrap();
        let mut txn = e.begin_write();
        txn.put_entity(ndb_engine::EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(1),
            tx_id_assert: ndb_engine::TxId::new(0),
            tx_id_supersede: ndb_engine::TxId::ACTIVE,
            properties: vec![(PropertyId::new(1), Value::I64(1))],
        });
        txn.commit().unwrap();
    }
    let addr = spawn(&server, 2);
    let cli = client(addr);

    let flush = cli.flush().unwrap();
    assert!(flush.sstable_count >= 1);

    let compact = cli.compact().unwrap();
    // With one SSTable, compaction is a no-op.
    assert_eq!(compact.sstables_in, 1);
    assert!(compact.new_sstable_seq.is_none());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn query_round_trip_entity_pattern() {
    use ndb_engine::{Pattern, PropertyFilter, QueryRequest, Term};

    const TYPE_CUSTOMER: u32 = 100;
    const PROP_NAME: u32 = 30;
    const PROP_REGION: u32 = 31;

    let dir = temp_dir("query");
    let server = Arc::new(Server::open(&dir).unwrap());
    {
        let e = server.engine();
        let mut e = e.lock().unwrap();
        for (name, region) in [
            ("Alice", "Vietnam"),
            ("Bob", "Singapore"),
            ("Carol", "Vietnam"),
        ] {
            let mut txn = e.begin_write();
            txn.put_entity(ndb_engine::EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: TypeId::new(TYPE_CUSTOMER),
                tx_id_assert: ndb_engine::TxId::new(0),
                tx_id_supersede: ndb_engine::TxId::ACTIVE,
                properties: vec![
                    (PropertyId::new(PROP_NAME), Value::String(name.into())),
                    (PropertyId::new(PROP_REGION), Value::String(region.into())),
                ],
            });
            txn.commit().unwrap();
        }
    }
    let addr = spawn(&server, 2);
    let cli = client(addr);

    let req = QueryRequest {
        as_of: None,
        patterns: vec![Pattern::Entity {
            type_id: TYPE_CUSTOMER,
            self_var: Some("c".into()),
            property_filters: vec![
                PropertyFilter {
                    property_id: PROP_REGION,
                    op: ndb_engine::CmpOp::Eq,
                    term: Term::Literal {
                        value: JsonValue::String {
                            value: "Vietnam".into(),
                        },
                    },
                },
                PropertyFilter {
                    property_id: PROP_NAME,
                    op: ndb_engine::CmpOp::Eq,
                    term: Term::Var { name: "n".into() },
                },
            ],
        }],
        filter: None,
        returns: vec!["c".into(), "n".into()],
        limit: None,
    };
    let resp = cli.query(&req).unwrap();
    assert_eq!(resp.columns, vec!["c", "n"]);
    assert_eq!(resp.rows.len(), 2);
    assert!(!resp.truncated);
    let names: std::collections::HashSet<String> = resp
        .rows
        .iter()
        .filter_map(|r| match &r[1] {
            JsonValue::String { value } => Some(value.clone()),
            _ => None,
        })
        .collect();
    assert!(names.contains("Alice"));
    assert!(names.contains("Carol"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn http_error_carries_status_and_detail() {
    let dir = temp_dir("http_err");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn(&server, 1);
    let cli = client(addr);

    let err = cli.read("not-a-uuid").unwrap_err();
    match err {
        ClientError::Http { status, .. } => assert_eq!(status, 400),
        other => panic!("expected Http 400, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn query_text_round_trip() {
    use ndb_engine::record::{PropertyKeyRecord, Record, RoleNameRecord, TypeNameRecord};

    const TYPE_CUSTOMER: u32 = 100;
    const PROP_NAME: u32 = 30;
    const PROP_REGION: u32 = 31;

    let dir = temp_dir("query-text");
    let server = Arc::new(Server::open(&dir).unwrap());
    {
        let e = server.engine();
        let mut e = e.lock().unwrap();

        // Dictionary names — the resolver needs these to map "customer"
        // and "name" / "region" identifiers to type / property ids.
        let mut txn = e.begin_write();
        txn.put_raw(Record::TypeName(TypeNameRecord {
            id: TypeId::new(TYPE_CUSTOMER), name: "customer".into(),
        }));
        txn.put_raw(Record::PropertyKey(PropertyKeyRecord {
            id: PropertyId::new(PROP_NAME), name: "name".into(),
        }));
        txn.put_raw(Record::PropertyKey(PropertyKeyRecord {
            id: PropertyId::new(PROP_REGION), name: "region".into(),
        }));
        let _: &RoleNameRecord;  // silence unused-import if no roles needed here
        txn.commit().unwrap();

        for (name, region) in [("Alice", "Vietnam"), ("Bob", "Singapore"), ("Carol", "Vietnam")] {
            let mut txn = e.begin_write();
            txn.put_entity(ndb_engine::EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: TypeId::new(TYPE_CUSTOMER),
                tx_id_assert: ndb_engine::TxId::new(0),
                tx_id_supersede: ndb_engine::TxId::ACTIVE,
                properties: vec![
                    (PropertyId::new(PROP_NAME), Value::String(name.into())),
                    (PropertyId::new(PROP_REGION), Value::String(region.into())),
                ],
            });
            txn.commit().unwrap();
        }
    }
    let addr = spawn(&server, 4);
    let cli = client(addr);

    // Happy path — filter by region literal inside the pattern.
    let resp = cli.query_text(
        r#"match customer(name: ?n, region: "Vietnam") as ?c return ?c, ?n"#
    ).unwrap();
    assert_eq!(resp.columns, vec!["c", "n"]);
    assert_eq!(resp.rows.len(), 2, "expected Alice + Carol, got {:?}", resp.rows);
    assert!(!resp.truncated);

    // Parse error — should surface as HTTP 400.
    let err = cli.query_text("this is not a query").unwrap_err();
    match err {
        ClientError::Http { status, .. } => assert_eq!(status, 400, "parse errors are client errors"),
        other => panic!("expected Http 400 for parse error, got {other:?}"),
    }

    // Unknown-type — resolver error, also 400.
    let err = cli.query_text("match planet() as ?p return ?p").unwrap_err();
    match err {
        ClientError::Http { status, detail, .. } => {
            assert_eq!(status, 400);
            assert!(detail.contains("unknown_type"), "detail should mention unknown_type: {detail}");
        }
        other => panic!("expected Http 400 for resolve error, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).unwrap();
}
