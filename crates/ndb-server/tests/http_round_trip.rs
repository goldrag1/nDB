//! End-to-end HTTP round-trip tests against the hand-rolled server.
#![allow(clippy::needless_pass_by_value, clippy::doc_markdown)]
//!
//! Strategy: pick an ephemeral port (`127.0.0.1:0`), spawn a worker
//! thread that serves a bounded number of connections via `serve_n`,
//! then issue requests via `std::net::TcpStream` and parse the responses
//! by hand. No external HTTP client dep.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ndb_engine::{EntityId, PropertyId, TypeId, Value, value::TAG_STRING};
use ndb_server::{Capability, Principal, Principals, Server};

fn temp_dir(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ndb-server-{}-{}",
        name,
        uuid::Uuid::now_v7().simple()
    ));
    p
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn raw_request(addr: std::net::SocketAddr, request: &[u8]) -> HttpResponse {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(request).unwrap();
    s.flush().unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header terminator");
    let head = &buf[..header_end];
    let body = buf[header_end + 4..].to_vec();
    let first_line = std::str::from_utf8(head).unwrap().lines().next().unwrap();
    // "HTTP/1.1 <code> <text>"
    let status: u16 = first_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    HttpResponse { status, body }
}

fn get(addr: std::net::SocketAddr, path: &str) -> HttpResponse {
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    raw_request(addr, req.as_bytes())
}

fn post(addr: std::net::SocketAddr, path: &str, json: &str) -> HttpResponse {
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        json.len(),
        json,
    );
    raw_request(addr, req.as_bytes())
}

fn spawn_server(server: Arc<Server>, n_conn: usize) -> std::net::SocketAddr {
    // Bind here so the test can read the actual port before the worker
    // thread starts.
    let bind = server.bind("127.0.0.1:0").unwrap();
    let addr = bind.local_addr().unwrap();
    let srv = Arc::clone(&server);
    thread::spawn(move || {
        // SAFETY: we re-bind inside the thread because BoundServer
        // borrows from the Server with a lifetime tied to the main
        // thread. Easier to just bind again than juggle scopes.
        let bind = srv.bind(addr).unwrap();
        let _ = bind.serve_n(n_conn);
    });
    // Give the thread a moment to start listening on the rebound addr.
    // Because we bound twice (once here for the port, once in the
    // thread), the inner bind will fail with "Address already in use"
    // unless we drop the outer first. Easiest fix: drop the outer bind
    // before sleeping. Note: `bind` goes out of scope at end of this fn.
    drop(bind);
    thread::sleep(Duration::from_millis(50));
    addr
}

#[test]
fn health_endpoint() {
    let dir = temp_dir("health");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn_server(Arc::clone(&server), 1);
    let resp = get(addr, "/health");
    assert_eq!(resp.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(body["status"], "ok");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn commit_then_read_round_trip() {
    let dir = temp_dir("commit_read");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn_server(Arc::clone(&server), 2);

    // Pre-register validation (server-side) — engine state survives
    // across requests.
    {
        let e = server.engine();
        let mut e = e.write().unwrap();
        e.require_property(TypeId::new(1), PropertyId::new(10));
        e.expect_value_tag(TypeId::new(1), PropertyId::new(10), TAG_STRING);
    }

    let alice = EntityId::now_v7();
    let commit_body = serde_json::json!({
        "records": [{
            "kind": "entity",
            "entity_id": alice.into_uuid().to_string(),
            "type_id": 1,
            "tx_id_assert": 0,
            "tx_id_supersede": "active",
            "properties": [{
                "prop_id": 10,
                "value": {"tag": "string", "value": "alice@example.com"}
            }]
        }]
    })
    .to_string();
    let resp = post(addr, "/commit", &commit_body);
    assert_eq!(
        resp.status,
        200,
        "commit body: {}",
        String::from_utf8_lossy(&resp.body)
    );
    let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert!(body["tx_id"].as_u64().unwrap() > 0);

    let resp = get(addr, &format!("/read/{}", alice.into_uuid()));
    assert_eq!(resp.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(body["outcome"], "live");
    assert_eq!(body["record"]["kind"], "entity");
    assert_eq!(body["record"]["entity_id"], alice.into_uuid().to_string());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn validation_failure_returns_422() {
    let dir = temp_dir("val_422");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn_server(Arc::clone(&server), 1);

    {
        let e = server.engine();
        let mut e = e.write().unwrap();
        e.require_property(TypeId::new(1), PropertyId::new(10));
    }

    let bad = serde_json::json!({
        "records": [{
            "kind": "entity",
            "entity_id": uuid::Uuid::now_v7().to_string(),
            "type_id": 1,
            "tx_id_assert": 0,
            "tx_id_supersede": "active",
            "properties": []
        }]
    })
    .to_string();
    let resp = post(addr, "/commit", &bad);
    assert_eq!(resp.status, 422);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(body["error"], "validation");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn auth_token_required_when_set() {
    let dir = temp_dir("auth_required");
    let server = Arc::new(
        Server::open(&dir)
            .unwrap()
            .with_auth_token("s3cr3t-token-x"),
    );
    let addr = spawn_server(Arc::clone(&server), 3);

    // No Authorization header → 401.
    let req = "GET /iter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let resp = raw_request(addr, req.as_bytes());
    assert_eq!(resp.status, 401);

    // Wrong token → 401.
    let req =
        "GET /iter HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\nConnection: close\r\n\r\n";
    let resp = raw_request(addr, req.as_bytes());
    assert_eq!(resp.status, 401);

    // Right token → 200.
    let req = "GET /iter HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer s3cr3t-token-x\r\nConnection: close\r\n\r\n";
    let resp = raw_request(addr, req.as_bytes());
    assert_eq!(resp.status, 200);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn health_open_even_with_auth_token() {
    let dir = temp_dir("auth_health");
    let server = Arc::new(Server::open(&dir).unwrap().with_auth_token("super-secret"));
    let addr = spawn_server(Arc::clone(&server), 1);
    // No Authorization, but /health is intentionally open.
    let resp = get(addr, "/health");
    assert_eq!(resp.status, 200);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_route_returns_404() {
    let dir = temp_dir("404");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn_server(Arc::clone(&server), 1);
    let resp = get(addr, "/nonexistent");
    assert_eq!(resp.status, 404);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn iter_streams_jsonl() {
    let dir = temp_dir("iter");
    let server = Arc::new(Server::open(&dir).unwrap());

    // Pre-load some records before the server starts serving.
    {
        let e = server.engine();
        let mut e = e.write().unwrap();
        for i in 0..3 {
            let mut txn = e.begin_write();
            txn.put_entity(ndb_engine::EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: TypeId::new(1),
                tx_id_assert: ndb_engine::TxId::new(0),
                tx_id_supersede: ndb_engine::TxId::ACTIVE,
                properties: vec![(PropertyId::new(1), Value::String(format!("entity-{i}")))],
            });
            txn.commit().unwrap();
        }
    }
    let addr = spawn_server(Arc::clone(&server), 1);

    let resp = get(addr, "/iter");
    assert_eq!(resp.status, 200);
    let body_str = std::str::from_utf8(&resp.body).unwrap();
    let lines: Vec<&str> = body_str.trim_end().split('\n').collect();
    assert_eq!(lines.len(), 3, "expected 3 records, body was {body_str:?}");
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["kind"], "entity");
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn lookup_route_finds_entity_by_external_key() {
    let dir = temp_dir("lookup");
    let server = Arc::new(Server::open(&dir).unwrap());

    let alice = EntityId::now_v7();
    {
        let e = server.engine();
        let mut e = e.write().unwrap();
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
    let addr = spawn_server(Arc::clone(&server), 2);

    let body = r#"{"property_id":10,"value":{"tag":"string","value":"alice@example.com"}}"#;
    let resp = post(addr, "/lookup", body);
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["entity_id"], alice.into_uuid().to_string());

    // Miss → entity_id null.
    let body = r#"{"property_id":10,"value":{"tag":"string","value":"nobody@example.com"}}"#;
    let resp = post(addr, "/lookup", body);
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert!(v["entity_id"].is_null());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn vector_search_route_returns_sorted_hits() {
    let dir = temp_dir("vec_search");
    let server = Arc::new(Server::open(&dir).unwrap());

    let a = EntityId::now_v7();
    let b = EntityId::now_v7();
    {
        let e = server.engine();
        let mut e = e.write().unwrap();
        e.register_vector_property(PropertyId::new(20));
        for (eid, v) in [(a, vec![1.0f32, 0.0]), (b, vec![0.0f32, 1.0])] {
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
    let addr = spawn_server(Arc::clone(&server), 2);

    let body = r#"{"property_id":20,"query":[1.0,0.0],"k":2,"metric":"l2"}"#;
    let resp = post(addr, "/vector_search", body);
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    let hits = v["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2);
    // Exact match comes first.
    assert_eq!(hits[0]["entity_id"], a.into_uuid().to_string());

    // k=0 is rejected.
    let resp = post(
        addr,
        "/vector_search",
        r#"{"property_id":20,"query":[1.0,0.0],"k":0,"metric":"l2"}"#,
    );
    assert_eq!(resp.status, 400);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn property_lookup_and_range_routes() {
    let dir = temp_dir("prop_btree");
    let server = Arc::new(Server::open(&dir).unwrap());

    let alice = EntityId::now_v7();
    let bob = EntityId::now_v7();
    let carol = EntityId::now_v7();
    {
        let e = server.engine();
        let mut e = e.write().unwrap();
        e.register_property_btree(TypeId::new(1), PropertyId::new(30));
        for (eid, age) in [(alice, 25i64), (bob, 35), (carol, 45)] {
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
    let addr = spawn_server(Arc::clone(&server), 3);

    // Exact match.
    let resp = post(
        addr,
        "/property_lookup",
        r#"{"type_id":1,"property_id":30,"value":{"tag":"i64","value":35}}"#,
    );
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    let ids = v["entity_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], bob.into_uuid().to_string());

    // Range 30..=50.
    let resp = post(
        addr,
        "/property_range",
        r#"{"type_id":1,"property_id":30,"low":{"tag":"i64","value":30},"high":{"tag":"i64","value":50}}"#,
    );
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    let ids = v["entity_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 2); // bob + carol

    // Unbounded low.
    let resp = post(
        addr,
        "/property_range",
        r#"{"type_id":1,"property_id":30,"high":{"tag":"i64","value":30}}"#,
    );
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    let ids = v["entity_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 1); // alice only

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn traverse_route_walks_2_hops() {
    const TYPE_PERSON: u32 = 1;
    const TYPE_KNOWS: u32 = 100;
    const TYPE_WORKS_AT: u32 = 101;

    let dir = temp_dir("traverse");
    let server = Arc::new(Server::open(&dir).unwrap());

    // Build a tiny graph:
    //   alice — (knows) — bob — (works_at) — acme
    // and walk: start=alice, hop=knows, hop=works_at, expect={acme}.
    let alice = EntityId::now_v7();
    let bob = EntityId::now_v7();
    let acme = EntityId::now_v7();
    {
        let e = server.engine();
        let mut e = e.write().unwrap();
        for eid in [alice, bob, acme] {
            let mut txn = e.begin_write();
            txn.put_entity(ndb_engine::EntityRecord {
                entity_id: eid,
                type_id: TypeId::new(TYPE_PERSON),
                tx_id_assert: ndb_engine::TxId::new(0),
                tx_id_supersede: ndb_engine::TxId::ACTIVE,
                properties: vec![],
            });
            txn.commit().unwrap();
        }
        // alice — knows — bob
        let mut txn = e.begin_write();
        txn.put_hyperedge(ndb_engine::HyperEdgeRecord {
            hyperedge_id: ndb_engine::HyperedgeId::now_v7(),
            type_id: TypeId::new(TYPE_KNOWS),
            tx_id_assert: ndb_engine::TxId::new(0),
            tx_id_supersede: ndb_engine::TxId::ACTIVE,
            roles: vec![
                (ndb_engine::RoleId::new(1), alice),
                (ndb_engine::RoleId::new(2), bob),
            ],
            hyperedge_roles: Vec::new(),
            properties: vec![],
        });
        txn.commit().unwrap();
        // bob — works_at — acme
        let mut txn = e.begin_write();
        txn.put_hyperedge(ndb_engine::HyperEdgeRecord {
            hyperedge_id: ndb_engine::HyperedgeId::now_v7(),
            type_id: TypeId::new(TYPE_WORKS_AT),
            tx_id_assert: ndb_engine::TxId::new(0),
            tx_id_supersede: ndb_engine::TxId::ACTIVE,
            roles: vec![
                (ndb_engine::RoleId::new(3), bob),
                (ndb_engine::RoleId::new(4), acme),
            ],
            hyperedge_roles: Vec::new(),
            properties: vec![],
        });
        txn.commit().unwrap();
    }
    let addr = spawn_server(Arc::clone(&server), 2);

    let body = format!(
        r#"{{"start":"{}","hops":[{{"hyperedge_type_id":{}}},{{"hyperedge_type_id":{}}}]}}"#,
        alice.into_uuid(),
        TYPE_KNOWS,
        TYPE_WORKS_AT,
    );
    let resp = post(addr, "/traverse", &body);
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    let ids = v["entity_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 1, "expected exactly acme; got {ids:?}");
    assert_eq!(ids[0], acme.into_uuid().to_string());

    // Bad-uuid start → 400.
    let resp = post(
        addr,
        "/traverse",
        r#"{"start":"not-a-uuid","hops":[]}"#,
    );
    assert_eq!(resp.status, 400);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn query_route_executes_entity_pattern_via_tcp() {
    const TYPE_CUSTOMER: u32 = 100;
    const PROP_NAME: u32 = 30;
    const PROP_REGION: u32 = 31;

    let dir = temp_dir("query");
    let server = Arc::new(Server::open(&dir).unwrap());

    // Seed three customers — two in Vietnam, one in Singapore.
    {
        let e = server.engine();
        let mut e = e.write().unwrap();
        for (name, region) in [
            ("Alice", "Vietnam"),
            ("Bob", "Singapore"),
            ("Carol", "Vietnam"),
        ] {
            let eid = EntityId::now_v7();
            let mut txn = e.begin_write();
            txn.put_entity(ndb_engine::EntityRecord {
                entity_id: eid,
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
    let addr = spawn_server(Arc::clone(&server), 3);

    // POST /query with an Entity pattern selecting Vietnam customers,
    // binding ?n to the name property.
    let body = format!(
        r#"{{
            "patterns": [{{
                "kind": "entity",
                "type_id": {TYPE_CUSTOMER},
                "self_var": "c",
                "property_filters": [
                    {{"property_id": {PROP_REGION}, "op": "eq",
                      "term": {{"kind":"literal","value":{{"tag":"string","value":"Vietnam"}}}} }},
                    {{"property_id": {PROP_NAME}, "op": "eq",
                      "term": {{"kind":"var","name":"n"}} }}
                ]
            }}],
            "returns": ["c", "n"]
        }}"#,
    );
    let resp = post(addr, "/query", &body);
    assert_eq!(resp.status, 200, "body = {:?}", String::from_utf8_lossy(&resp.body));
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    let columns = v["columns"].as_array().unwrap();
    assert_eq!(columns, &[serde_json::json!("c"), serde_json::json!("n")]);
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "two Vietnam customers expected: {rows:?}");
    // Names must be Alice or Carol; UUIDs must be valid.
    let names: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|r| r[1]["value"].as_str().map(str::to_string))
        .collect();
    assert!(names.contains("Alice"), "got {names:?}");
    assert!(names.contains("Carol"), "got {names:?}");
    for r in rows {
        assert_eq!(r[0]["tag"], "uuid");
        assert_eq!(r[1]["tag"], "string");
    }

    // Bad body → 400.
    let resp = post(addr, "/query", "not json");
    assert_eq!(resp.status, 400);

    // Recursion with no endpoint roles → 400 recursion_config_invalid.
    let body = format!(
        r#"{{
            "patterns": [{{
                "kind": "hyperedge",
                "type_id": {TYPE_CUSTOMER},
                "recursion": {{"kind": "star", "max_depth": 10}}
            }}],
            "returns": []
        }}"#,
    );
    let resp = post(addr, "/query", &body);
    assert_eq!(resp.status, 400);
    let parsed: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(parsed["error"], "recursion_config_invalid");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn query_explain_returns_plan_tree_without_executing() {
    const TYPE_CUSTOMER: u32 = 100;
    const TYPE_PURCHASE: u32 = 200;
    const ROLE_BUYER: u32 = 10;
    const PROP_NAME: u32 = 30;
    const PROP_REGION: u32 = 31;

    let dir = temp_dir("query-explain");
    let server = Arc::new(Server::open(&dir).unwrap());
    {
        let e = server.engine();
        let mut e = e.write().unwrap();
        // Names so parse_resolve succeeds against the engine snapshot.
        let mut tx = e.begin_write();
        tx.put_raw(ndb_engine::Record::TypeName(ndb_engine::TypeNameRecord {
            id: TypeId::new(TYPE_CUSTOMER),
            name: "customer".into(),
        }));
        tx.put_raw(ndb_engine::Record::TypeName(ndb_engine::TypeNameRecord {
            id: TypeId::new(TYPE_PURCHASE),
            name: "purchase".into(),
        }));
        tx.put_raw(ndb_engine::Record::RoleName(ndb_engine::RoleNameRecord {
            id: ndb_engine::RoleId::new(ROLE_BUYER),
            name: "buyer".into(),
        }));
        tx.put_raw(ndb_engine::Record::PropertyKey(ndb_engine::PropertyKeyRecord {
            id: PropertyId::new(PROP_NAME),
            name: "name".into(),
        }));
        tx.put_raw(ndb_engine::Record::PropertyKey(ndb_engine::PropertyKeyRecord {
            id: PropertyId::new(PROP_REGION),
            name: "region".into(),
        }));
        tx.commit().unwrap();
        // Seed: register region B-tree + 1 Vietnam customer (Alice) + 10
        // Singapore fillers + 5 purchases tied to Alice. Entity B-tree
        // probe is then a strict 1; hyperedge type-cluster is 5. Planner
        // must seed the entity, bind ?c, then walk adjacency for the
        // purchase pattern.
        e.register_property_btree(TypeId::new(TYPE_CUSTOMER), PropertyId::new(PROP_REGION));
        let alice = EntityId::now_v7();
        let mut tx = e.begin_write();
        tx.put_entity(ndb_engine::EntityRecord {
            entity_id: alice,
            type_id: TypeId::new(TYPE_CUSTOMER),
            tx_id_assert: ndb_engine::TxId::new(0),
            tx_id_supersede: ndb_engine::TxId::ACTIVE,
            properties: vec![
                (PropertyId::new(PROP_NAME), Value::String("Alice".into())),
                (PropertyId::new(PROP_REGION), Value::String("Vietnam".into())),
            ],
        });
        tx.commit().unwrap();
        for _ in 0..10 {
            let mut tx = e.begin_write();
            tx.put_entity(ndb_engine::EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: TypeId::new(TYPE_CUSTOMER),
                tx_id_assert: ndb_engine::TxId::new(0),
                tx_id_supersede: ndb_engine::TxId::ACTIVE,
                properties: vec![
                    (PropertyId::new(PROP_NAME), Value::String("filler".into())),
                    (PropertyId::new(PROP_REGION), Value::String("Singapore".into())),
                ],
            });
            tx.commit().unwrap();
        }
        for _ in 0..5 {
            let mut tx = e.begin_write();
            tx.put_hyperedge(ndb_engine::HyperEdgeRecord {
                hyperedge_id: ndb_engine::HyperedgeId::now_v7(),
                type_id: TypeId::new(TYPE_PURCHASE),
                tx_id_assert: ndb_engine::TxId::new(0),
                tx_id_supersede: ndb_engine::TxId::ACTIVE,
                roles: vec![(ndb_engine::RoleId::new(ROLE_BUYER), alice)],
                hyperedge_roles: Vec::new(),
                properties: vec![],
            });
            tx.commit().unwrap();
        }
    }
    let addr = spawn_server(Arc::clone(&server), 3);

    // Two-pattern join. Source order is [purchase, customer]; planner
    // should reorder to [customer (cardinality=2, indexed), purchase
    // (adjacency-bound via ?c)].
    let body = r#"match purchase(buyer: ?c) customer(region: "Vietnam") as ?c return ?c"#;
    let req = format!(
        "POST /query/explain HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    let resp = raw_request(addr, req.as_bytes());
    assert_eq!(resp.status, 200, "body = {:?}", String::from_utf8_lossy(&resp.body));
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["patterns"], 2);
    let plan = v["plan"].as_array().expect("plan array");
    assert_eq!(plan.len(), 2);
    // Planned first entry is the entity pattern (lower cardinality).
    assert_eq!(plan[0]["pattern_index"], 1, "entity must seed; got {plan:?}");
    assert_eq!(plan[0]["estimated_cardinality"], 1);
    assert!(plan[0]["atom_summary"].as_str().unwrap().starts_with("entity"));
    let binds: Vec<&str> = plan[0]["binds"].as_array().unwrap()
        .iter().map(|x| x.as_str().unwrap()).collect();
    assert!(binds.contains(&"c"), "seed must bind ?c; got {binds:?}");
    // Second entry references ?c via uses.
    let uses1: Vec<&str> = plan[1]["uses"].as_array().unwrap()
        .iter().map(|x| x.as_str().unwrap()).collect();
    assert!(uses1.contains(&"c"));
    assert!(plan[1]["atom_summary"].as_str().unwrap().starts_with("hyperedge"));

    // Parse error → 400 + envelope shape.
    let bad = "match no closing paren";
    let req = format!(
        "POST /query/explain HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        bad.len(),
        bad,
    );
    let resp = raw_request(addr, req.as_bytes());
    assert_eq!(resp.status, 400);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["error"], "parse");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn subscribe_returns_records_committed_after_since_tx() {
    const TYPE_ITEM: u32 = 100;

    let dir = temp_dir("subscribe");
    let server = Arc::new(Server::open(&dir).unwrap());
    let since_tx_id;
    {
        let e = server.engine();
        let mut e = e.write().unwrap();
        // Commit one entity → bumps last_tx_id to 1.
        let mut txn = e.begin_write();
        txn.put_entity(ndb_engine::EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(TYPE_ITEM),
            tx_id_assert: ndb_engine::TxId::new(0),
            tx_id_supersede: ndb_engine::TxId::ACTIVE,
            properties: vec![],
        });
        txn.commit().unwrap();
        since_tx_id = e.manifest().last_tx_id;
        // Two more — those should appear in subscribe.
        for _ in 0..2 {
            let mut txn = e.begin_write();
            txn.put_entity(ndb_engine::EntityRecord {
                entity_id: EntityId::now_v7(),
                type_id: TypeId::new(TYPE_ITEM),
                tx_id_assert: ndb_engine::TxId::new(0),
                tx_id_supersede: ndb_engine::TxId::ACTIVE,
                properties: vec![],
            });
            txn.commit().unwrap();
        }
    }
    let addr = spawn_server(Arc::clone(&server), 1);

    let body = format!(r#"{{"since_tx_id": {since_tx_id}, "timeout_ms": 500}}"#);
    let resp = post(addr, "/subscribe", &body);
    assert_eq!(resp.status, 200);
    let text = std::str::from_utf8(&resp.body).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(!lines.is_empty());
    let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert!(header["current_tx_id"].as_u64().unwrap() > since_tx_id);
    // The 2 newer entities both have tx_id_assert > since_tx_id → 2 record lines.
    let record_lines = &lines[1..];
    assert_eq!(
        record_lines.len(),
        2,
        "expected 2 newer records, got {record_lines:?}"
    );
    for rl in record_lines {
        let r: serde_json::Value = serde_json::from_str(rl).unwrap();
        assert_eq!(r["kind"], "entity");
        assert!(r["tx_id_assert"].as_u64().unwrap() > since_tx_id);
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn subscribe_wakes_on_concurrent_commit_within_a_millisecond_class_latency() {
    // Pin a subscriber to a known tx_id, then commit concurrently from
    // a sibling thread. With v2.1's per-connection thread spawn the
    // subscriber's condvar wait no longer blocks the next accept,
    // so the wake latency from notify is sub-ms on localhost.
    //
    // The subscriber thread's elapsed time = ~50ms intentional sleep
    // on the main thread (waiting for the subscriber to reach the
    // condvar) + wake-latency. We assert wake-latency < 50ms (the
    // old polling-bound floor); on a healthy local box it's <1ms.
    use std::time::{Duration, Instant};

    const TYPE_ITEM: u32 = 100;
    const PRE_COMMIT_SLEEP_MS: u64 = 50;
    let dir = temp_dir("subscribe-condvar");
    let server = Arc::new(Server::open(&dir).unwrap());
    let since_tx_id;
    {
        let e = server.engine();
        let mut e = e.write().unwrap();
        let mut txn = e.begin_write();
        txn.put_entity(ndb_engine::EntityRecord {
            entity_id: EntityId::now_v7(),
            type_id: TypeId::new(TYPE_ITEM),
            tx_id_assert: ndb_engine::TxId::new(0),
            tx_id_supersede: ndb_engine::TxId::ACTIVE,
            properties: vec![],
        });
        txn.commit().unwrap();
        since_tx_id = e.manifest().last_tx_id;
    }
    let addr = spawn_server(Arc::clone(&server), 2);

    // Spawn the subscriber FIRST so it's blocking on the condvar
    // before the commit lands.
    let body = format!(r#"{{"since_tx_id": {since_tx_id}, "timeout_ms": 5000}}"#);
    let sub_handle = std::thread::spawn(move || {
        let started = Instant::now();
        let resp = post(addr, "/subscribe", &body);
        (started.elapsed(), resp)
    });
    // Give the subscriber a beat to reach the condvar wait. v2.1's
    // per-connection thread spawn means /commit no longer queues
    // behind it.
    std::thread::sleep(Duration::from_millis(PRE_COMMIT_SLEEP_MS));
    let commit_body = format!(
        r#"{{"records":[{{"kind":"entity","entity_id":"{}","type_id":{TYPE_ITEM},"tx_id_assert":0,"tx_id_supersede":"active","properties":[]}}]}}"#,
        EntityId::now_v7().into_uuid(),
    );
    let commit_resp = post(addr, "/commit", &commit_body);
    assert_eq!(commit_resp.status, 200);

    let (elapsed, sub_resp) = sub_handle.join().unwrap();
    assert_eq!(sub_resp.status, 200);
    // Subscriber's elapsed = pre-commit sleep + wake latency + tiny
    // overhead. We want to assert wake latency itself is < 50ms.
    let wake_latency = elapsed.saturating_sub(Duration::from_millis(PRE_COMMIT_SLEEP_MS));
    assert!(
        wake_latency < Duration::from_millis(50),
        "wake latency {wake_latency:?} should be < 50ms; total elapsed = {elapsed:?}"
    );
    let text = std::str::from_utf8(&sub_resp.body).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() >= 2, "header + at least one record");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn subscribe_times_out_with_only_header_when_no_new_commits() {
    let dir = temp_dir("subscribe-timeout");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn_server(Arc::clone(&server), 1);
    let body = r#"{"since_tx_id": 0, "timeout_ms": 200}"#;
    let started = std::time::Instant::now();
    let resp = post(addr, "/subscribe", body);
    let elapsed = started.elapsed();
    assert_eq!(resp.status, 200);
    // Timeout polling should take at least timeout_ms.
    assert!(
        elapsed >= std::time::Duration::from_millis(200),
        "elapsed = {elapsed:?}"
    );
    let text = std::str::from_utf8(&resp.body).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "expected only header, got {lines:?}");
    let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(header["current_tx_id"], 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn query_stream_emits_header_plus_one_line_per_row() {
    const TYPE_CUSTOMER: u32 = 100;
    const PROP_NAME: u32 = 30;
    const PROP_REGION: u32 = 31;

    let dir = temp_dir("qstream");
    let server = Arc::new(Server::open(&dir).unwrap());
    {
        let e = server.engine();
        let mut e = e.write().unwrap();
        for n in 0..4 {
            let eid = EntityId::now_v7();
            let mut txn = e.begin_write();
            txn.put_entity(ndb_engine::EntityRecord {
                entity_id: eid,
                type_id: TypeId::new(TYPE_CUSTOMER),
                tx_id_assert: ndb_engine::TxId::new(0),
                tx_id_supersede: ndb_engine::TxId::ACTIVE,
                properties: vec![
                    (PropertyId::new(PROP_NAME), Value::String(format!("c{n}"))),
                    (PropertyId::new(PROP_REGION), Value::String("Vietnam".into())),
                ],
            });
            txn.commit().unwrap();
        }
    }
    let addr = spawn_server(Arc::clone(&server), 1);

    let body = format!(
        r#"{{
            "patterns": [{{
                "kind": "entity",
                "type_id": {TYPE_CUSTOMER},
                "self_var": "c",
                "property_filters": [
                    {{"property_id": {PROP_REGION}, "op": "eq",
                      "term": {{"kind":"literal","value":{{"tag":"string","value":"Vietnam"}}}} }},
                    {{"property_id": {PROP_NAME}, "op": "eq",
                      "term": {{"kind":"var","name":"n"}} }}
                ]
            }}],
            "returns": ["c", "n"]
        }}"#,
    );
    let resp = post(addr, "/query_stream", &body);
    assert_eq!(resp.status, 200);
    let text = std::str::from_utf8(&resp.body).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    // Line 0: header. Lines 1..=4: four rows.
    assert!(lines.len() >= 5, "expected ≥5 lines, got {}", lines.len());
    let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(header["columns"], serde_json::json!(["c", "n"]));
    assert_eq!(header["truncated"], serde_json::json!(false));
    // Every subsequent line is a JSON array of two JsonValue objects.
    for row_line in &lines[1..] {
        let row: serde_json::Value = serde_json::from_str(row_line).unwrap();
        let arr = row.as_array().expect("row is an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["tag"], "uuid");
        assert_eq!(arr[1]["tag"], "string");
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn malformed_uuid_in_read_returns_400() {
    let dir = temp_dir("bad_uuid");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn_server(Arc::clone(&server), 1);
    let resp = get(addr, "/read/not-a-uuid");
    assert_eq!(resp.status, 400);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn audit_log_records_each_request() {
    let dir = temp_dir("audit");
    let server = Arc::new(Server::open(&dir).unwrap().with_audit_log().unwrap());
    let addr = spawn_server(Arc::clone(&server), 2);
    let r1 = get(addr, "/health");
    assert_eq!(r1.status, 200);
    let r2 = get(addr, "/nonexistent");
    assert_eq!(r2.status, 404);

    let audit_path = server.audit_log_path().expect("audit enabled");
    let bytes = std::fs::read(&audit_path).expect("audit file");
    let s = std::str::from_utf8(&bytes).unwrap();
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 audit lines, got: {s:?}");
    let row1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(row1["path"], "/health");
    assert_eq!(row1["status"], 200);
    assert_eq!(row1["method"], "GET");
    let row2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(row2["path"], "/nonexistent");
    assert_eq!(row2["status"], 404);
    assert!(row2["failure"].is_string());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn principals_403_when_token_lacks_capability() {
    use std::collections::BTreeSet;
    let dir = temp_dir("rebac_403");
    let mut p = Principals::default();
    p.principals.insert(
        "reader-token".into(),
        Principal {
            name: "alice-readonly".into(),
            capabilities: BTreeSet::from([Capability::Read, Capability::Iter]),
            entity_id: None,
        },
    );
    let server = Arc::new(Server::open(&dir).unwrap().with_principals(p));
    let addr = spawn_server(Arc::clone(&server), 2);

    // /iter is allowed.
    let req = b"GET /iter HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer reader-token\r\nConnection: close\r\n\r\n";
    let resp = raw_request(addr, req);
    assert_eq!(resp.status, 200);

    // /flush is admin-tier; expect 403.
    let req = b"POST /flush HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer reader-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let resp = raw_request(addr, req);
    assert_eq!(resp.status, 403);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert!(body["detail"].as_str().unwrap().contains("alice-readonly"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn principals_admin_capability_overrides_others() {
    use std::collections::BTreeSet;
    let dir = temp_dir("rebac_admin");
    let mut p = Principals::default();
    p.principals.insert(
        "root-token".into(),
        Principal {
            name: "root".into(),
            capabilities: BTreeSet::from([Capability::Admin]),
            entity_id: None,
        },
    );
    let server = Arc::new(Server::open(&dir).unwrap().with_principals(p));
    let addr = spawn_server(Arc::clone(&server), 1);

    let req = b"POST /flush HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer root-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let resp = raw_request(addr, req);
    assert_eq!(resp.status, 200);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn principals_unknown_token_401() {
    let dir = temp_dir("rebac_unknown");
    let p = Principals::default();
    let server = Arc::new(Server::open(&dir).unwrap().with_principals(p));
    let addr = spawn_server(Arc::clone(&server), 1);
    let req = b"GET /iter HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer not-a-real-token\r\nConnection: close\r\n\r\n";
    let resp = raw_request(addr, req);
    assert_eq!(resp.status, 401);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn principals_health_open_even_without_token() {
    use std::collections::BTreeSet;
    let dir = temp_dir("rebac_health");
    let mut p = Principals::default();
    p.principals.insert(
        "tok".into(),
        Principal {
            name: "alice".into(),
            capabilities: BTreeSet::from([Capability::Read]),
            entity_id: None,
        },
    );
    let server = Arc::new(Server::open(&dir).unwrap().with_principals(p));
    let addr = spawn_server(Arc::clone(&server), 1);
    let resp = get(addr, "/health");
    assert_eq!(resp.status, 200);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn tls_round_trip_health() {
    use rcgen::generate_simple_self_signed;
    use std::io::Read as _;
    use std::sync::Arc;

    let dir = temp_dir("tls");

    // Generate self-signed cert for localhost.
    let subject_alt_names = vec!["localhost".to_string()];
    let cert = generate_simple_self_signed(subject_alt_names).expect("rcgen");
    let cert_path = dir.join(".test-cert.pem");
    let key_path = dir.join(".test-key.pem");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    let server = Arc::new(
        Server::open(&dir)
            .unwrap()
            .with_tls_pem(&cert_path, &key_path)
            .expect("tls config"),
    );
    let bind = server.bind_tls("127.0.0.1:0").unwrap();
    let addr = bind.local_addr().unwrap();
    drop(bind);

    let srv = Arc::clone(&server);
    thread::spawn(move || {
        let bind = srv.bind_tls(addr).unwrap();
        let _ = bind.serve_n(1);
    });
    thread::sleep(Duration::from_millis(100));

    // Client side: use rustls to dial.
    let der_cert = cert.cert.der().clone();
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add(der_cert).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .unwrap()
        .to_owned();
    let mut conn = rustls::ClientConnection::new(Arc::new(cfg), server_name).unwrap();
    let mut sock = std::net::TcpStream::connect(addr).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);

    let req = b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    tls.write_all(req).unwrap();
    let mut buf = Vec::new();
    tls.read_to_end(&mut buf).ok(); // close_notify may be present or absent
    let text = std::str::from_utf8(&buf).unwrap();
    assert!(text.starts_with("HTTP/1.1 200"), "got: {text:?}");
    assert!(text.contains("\"status\":\"ok\""), "got: {text:?}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn principals_load_from_disk() {
    use std::collections::BTreeSet;
    let dir = temp_dir("rebac_disk");
    let server = Server::open(&dir).unwrap();
    // Drop the engine borrow before writing the file.
    drop(server);

    let principals = Principals {
        principals: std::collections::HashMap::from([(
            "from-disk-token".to_string(),
            Principal {
                name: "disk-user".into(),
                capabilities: BTreeSet::from([Capability::Iter]),
            entity_id: None,
            },
        )]),
    };
    let principals_path = dir.join(".principals.json");
    std::fs::write(
        &principals_path,
        serde_json::to_vec_pretty(&principals).unwrap(),
    )
    .unwrap();

    let (server, loaded) = Server::open(&dir)
        .unwrap()
        .with_principals_from_db()
        .unwrap();
    assert!(loaded, "expected principals file to be loaded");
    let server = Arc::new(server);
    let addr = spawn_server(Arc::clone(&server), 1);

    let req = b"GET /iter HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer from-disk-token\r\nConnection: close\r\n\r\n";
    let resp = raw_request(addr, req);
    assert_eq!(resp.status, 200);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn audit_log_records_principal_when_token_set() {
    let dir = temp_dir("audit_tok");
    let server = Arc::new(
        Server::open(&dir)
            .unwrap()
            .with_auth_token("the-secret-token")
            .with_audit_log()
            .unwrap(),
    );
    let addr = spawn_server(Arc::clone(&server), 1);

    let req = b"GET /iter HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer the-secret-token\r\nConnection: close\r\n\r\n";
    let resp = raw_request(addr, req);
    assert_eq!(resp.status, 200);

    let path = server.audit_log_path().unwrap();
    let s = std::fs::read_to_string(&path).unwrap();
    let row: serde_json::Value = serde_json::from_str(s.lines().next().unwrap()).unwrap();
    let principal = row["principal"].as_str().unwrap();
    assert!(
        principal.starts_with("token:"),
        "principal should be hashed token id, got {principal}",
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// v2.0 #25: capability hyperedges as persistent ReBAC store
// ---------------------------------------------------------------------------

#[test]
fn principals_bootstrap_imports_json_into_engine_capability_hyperedges() {
    // Goal: prove the bootstrap flow round-trips. First open with
    // .principals.json present + no capability hyperedges in the engine
    // ⇒ imports. Second open with the file deleted ⇒ rebuilds the
    // Principals cache from engine queries; auth still works.
    use std::collections::BTreeSet;

    let dir = temp_dir("principals-bootstrap");
    std::fs::create_dir_all(&dir).unwrap();

    // Write a principals.json with two principals.
    let principals_json = serde_json::json!({
        "principals": {
            "alice-token": {
                "name": "Alice",
                "capabilities": ["read", "iter"]
            },
            "bob-token": {
                "name": "Bob",
                "capabilities": ["admin"]
            }
        }
    });
    std::fs::write(
        dir.join(".principals.json"),
        serde_json::to_string_pretty(&principals_json).unwrap(),
    )
    .unwrap();

    // First open — bootstrap should import 3 capability hyperedges
    // (2 for alice, 1 for bob).
    let (server, n_imported) = Server::open(&dir)
        .unwrap()
        .with_principals_bootstrapped()
        .unwrap();
    assert_eq!(n_imported, 3, "expected 3 capability hyperedges imported");

    // Confirm the principals cache reflects the imported set.
    let p = server.principals_for_test();
    assert_eq!(p.principals.len(), 2);
    let alice = p.principals.get("alice-token").unwrap();
    assert_eq!(alice.name, "Alice");
    let expected_alice: BTreeSet<Capability> =
        [Capability::Read, Capability::Iter].into_iter().collect();
    assert_eq!(alice.capabilities, expected_alice);

    let bob = p.principals.get("bob-token").unwrap();
    assert!(bob.capabilities.contains(&Capability::Admin));
    drop(server);

    // Second open — delete the file. with_principals_bootstrapped should
    // be a no-op import (engine already populated) but still rebuild the
    // cache from engine.
    std::fs::remove_file(dir.join(".principals.json")).unwrap();
    let (server2, n_imported2) = Server::open(&dir)
        .unwrap()
        .with_principals_bootstrapped()
        .unwrap();
    assert_eq!(n_imported2, 0, "engine already populated — no re-import");
    let p2 = server2.principals_for_test();
    assert_eq!(p2.principals.len(), 2);
    assert!(p2.principals.contains_key("alice-token"));
    assert!(p2.principals.contains_key("bob-token"));

    drop(server2);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn principals_bootstrap_no_file_is_empty_registry() {
    // Bootstrap on an engine with no .principals.json + no engine state
    // should install an empty registry (= no auth gating).
    let dir = temp_dir("principals-bootstrap-empty");
    std::fs::create_dir_all(&dir).unwrap();
    let (server, n_imported) = Server::open(&dir)
        .unwrap()
        .with_principals_bootstrapped()
        .unwrap();
    assert_eq!(n_imported, 0);
    let p = server.principals_for_test();
    assert!(p.principals.is_empty());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn engine_backed_dispatch_revokes_capability_without_restart() {
    // v2.1 §2.2 — capability revoked via engine commit takes effect on
    // the very next request. No server restart.
    use ndb_engine::{
        HyperedgeId, PROP_ACTION, Record, Resolved, ROLE_SUBJECT, TYPE_CAPABILITY, TxId,
    };

    let dir = temp_dir("auth-engine-dispatch");
    std::fs::create_dir_all(&dir).unwrap();
    let principals_json = serde_json::json!({
        "principals": {
            "alice-token": { "name": "Alice", "capabilities": ["iter"] }
        }
    });
    std::fs::write(
        dir.join(".principals.json"),
        serde_json::to_string_pretty(&principals_json).unwrap(),
    )
    .unwrap();

    let (server, n_imported) = Server::open(&dir)
        .unwrap()
        .with_principals_bootstrapped()
        .unwrap();
    assert_eq!(n_imported, 1);

    // Sanity: cache carries an entity_id (engine-backed path).
    let alice_eid = server
        .principals_for_test()
        .principals
        .get("alice-token")
        .and_then(|p| p.entity_id)
        .expect("entity_id populated by bootstrap");

    let server = Arc::new(server);
    let addr = spawn_server(Arc::clone(&server), 2);

    // First request — capability present.
    let req_with_token = b"GET /iter HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer alice-token\r\nConnection: close\r\n\r\n";
    let resp = raw_request(addr, req_with_token);
    assert_eq!(resp.status, 200, "first /iter must succeed");

    // Revoke alice's "iter" capability — find the hyperedge incident on
    // alice with PROP_ACTION="iter" and commit a tombstone for it.
    {
        let eng_arc = server.engine();
        let mut eng = eng_arc.write().unwrap();
        let snap = TxId::new(eng.manifest().last_tx_id);
        let edges: Vec<HyperedgeId> = eng.hyperedges_for_entity(alice_eid);
        let mut iter_hid: Option<HyperedgeId> = None;
        for hid in &edges {
            if let Ok(Resolved::Live(Record::HyperEdge(h))) =
                eng.snapshot_read(&hid.into_uuid(), snap)
                && h.type_id == TYPE_CAPABILITY
                && h.roles.iter().any(|(r, e)| *r == ROLE_SUBJECT && *e == alice_eid)
                && h.properties.iter().any(|(p, v)| {
                    *p == PROP_ACTION
                        && matches!(v, ndb_engine::Value::String(s) if s == "iter")
                })
            {
                iter_hid = Some(*hid);
                break;
            }
        }
        let iter_hid = iter_hid.expect("alice has an iter capability hyperedge");
        let mut txn = eng.begin_write();
        txn.delete(iter_hid.into_uuid());
        txn.commit().unwrap();
    }

    // Second request — must 403 without restart.
    // (Re-bind a new bounded server because spawn_server's serve_n(2)
    //  used up both connections already.)
    let addr2 = spawn_server(Arc::clone(&server), 1);
    let resp = raw_request(addr2, req_with_token);
    assert_eq!(resp.status, 403, "post-revocation /iter must 403; got {}", resp.status);

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// v2.2 preview — CORS preflight + ACAO header injection
// ---------------------------------------------------------------------------

fn raw_full(addr: std::net::SocketAddr, req: &[u8]) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(req).unwrap();
    s.flush().unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = text
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, text)
}

#[test]
fn cors_options_preflight_returns_204_with_acao() {
    let dir = temp_dir("cors_preflight");
    let server = Arc::new(Server::open(&dir).unwrap().with_cors_origin("*"));
    let addr = spawn_server(Arc::clone(&server), 1);
    let req = b"OPTIONS /commit HTTP/1.1\r\nHost: x\r\nOrigin: http://example.com\r\nAccess-Control-Request-Method: POST\r\nConnection: close\r\n\r\n";
    let (status, raw) = raw_full(addr, req);
    assert_eq!(status, 204);
    assert!(raw.contains("Access-Control-Allow-Origin: *"));
    assert!(raw.contains("Access-Control-Allow-Methods:"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cors_get_response_carries_acao_header() {
    let dir = temp_dir("cors_get");
    let server = Arc::new(Server::open(&dir).unwrap().with_cors_origin("*"));
    let addr = spawn_server(Arc::clone(&server), 1);
    let (status, raw) = raw_full(
        addr,
        b"GET /health HTTP/1.1\r\nHost: x\r\nOrigin: http://localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 200);
    assert!(
        raw.contains("Access-Control-Allow-Origin: *"),
        "missing ACAO header. Raw response:\n{raw}"
    );
    // Body still parses as the health JSON.
    assert!(raw.contains("\"status\":\"ok\""));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cors_disabled_by_default() {
    let dir = temp_dir("cors_default_off");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn_server(Arc::clone(&server), 1);
    let (status, raw) = raw_full(
        addr,
        b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 200);
    assert!(
        !raw.to_lowercase().contains("access-control-allow-origin"),
        "expected no CORS headers; got:\n{raw}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

// ===========================================================================
// P1/P2 hardening: resource limits, observability, graceful shutdown.
// (Added tests only — all helpers above are reused.)
// ===========================================================================

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use ndb_server::ShutdownHandle;

/// Send a raw request and read whatever response comes back, tolerating a
/// connection reset (the capacity-rejection path closes the socket promptly,
/// which can RST a still-writing client — the 503 bytes are usually already
/// buffered client-side before the RST). Returns `(status, full_text)`, or
/// `None` if nothing at all was read.
fn raw_full_tolerant(addr: std::net::SocketAddr, req: &[u8]) -> Option<(u16, String)> {
    let mut s = TcpStream::connect(addr).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    // A write failure here means the server already closed/reset — fall
    // through to the read, which may still surface the 503 the server wrote.
    let _ = s.write_all(req);
    let _ = s.flush();
    let mut buf = Vec::new();
    // Reset mid-stream is acceptable; keep whatever bytes we got.
    let _ = s.read_to_end(&mut buf);
    if buf.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = text
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((status, text))
}

/// Spawn a server that serves until graceful shutdown (the new
/// `serve_forever` path), returning the address + a handle to stop it.
/// The worker thread is detached; tests that need it to finish call
/// `request_shutdown` (or hit `/admin/shutdown`).
fn spawn_server_forever(server: Arc<Server>) -> (std::net::SocketAddr, ShutdownHandle) {
    let bind = server.bind("127.0.0.1:0").unwrap();
    let addr = bind.local_addr().unwrap();
    let handle = server.shutdown_handle();
    drop(bind);
    let srv = Arc::clone(&server);
    thread::spawn(move || {
        let bind = srv.bind(addr).unwrap();
        let _ = bind.serve_forever();
    });
    thread::sleep(Duration::from_millis(50));
    (addr, handle)
}

#[test]
fn connection_cap_rejects_with_503_then_recovers() {
    // max_connections=1: while one slow connection holds the only slot,
    // a second connection must be rejected with 503.
    let dir = temp_dir("conn_cap");
    let server = Arc::new(
        Server::open(&dir)
            .unwrap()
            .with_max_connections(1)
            // Generous read timeout so the slow connection holds its slot
            // for the duration of the test rather than timing out early.
            .with_timeouts(Duration::from_secs(5), Duration::from_secs(5)),
    );
    let (addr, handle) = spawn_server_forever(Arc::clone(&server));

    // Connection A: announce a body but never send it. The server thread
    // blocks reading the body, holding the single slot.
    let mut slow = TcpStream::connect(addr).unwrap();
    slow.write_all(
        b"POST /commit HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 64\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    slow.flush().unwrap();

    // Wait until the slot is actually taken (in_flight == 1).
    let deadline = Instant::now() + Duration::from_secs(2);
    while server.in_flight() == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(server.in_flight(), 1, "slow connection should hold the slot");

    // Connection B: a full request → rejected with 503 because we're at cap.
    // The rejection path closes the socket promptly, so tolerate a RST.
    let (status, raw) = raw_full_tolerant(
        addr,
        b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .expect("capacity rejection should have produced a 503 response");
    assert_eq!(status, 503, "expected capacity rejection; got:\n{raw}");
    assert!(raw.contains("capacity"), "rejection body should mention capacity:\n{raw}");

    // The rejected counter should have advanced.
    let metrics = server.metrics();
    assert!(
        metrics.render().contains("ndb_connections_rejected_total 1"),
        "rejected counter not incremented:\n{}",
        metrics.render(),
    );

    // Release the slow connection; the slot should free up.
    drop(slow);
    let deadline = Instant::now() + Duration::from_secs(6);
    while server.in_flight() > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(server.in_flight(), 0, "slot should free after slow conn closes");

    // And a fresh request now succeeds.
    let resp = get(addr, "/health");
    assert_eq!(resp.status, 200);

    handle.request();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn body_size_limit_returns_413() {
    let dir = temp_dir("body_limit");
    let server = Arc::new(
        Server::open(&dir)
            .unwrap()
            // 16-byte body cap; anything larger is refused pre-read.
            .with_request_limits(64 * 1024, 16),
    );
    let addr = spawn_server(Arc::clone(&server), 1);

    // Content-Length exceeds the cap → 413 without the body ever being read.
    let body = "x".repeat(100);
    let req = format!(
        "POST /commit HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    let (status, raw) = raw_full(addr, req.as_bytes());
    assert_eq!(status, 413, "expected 413 body too large; got:\n{raw}");
    assert!(raw.contains("body_too_large"), "error code missing:\n{raw}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn header_size_limit_returns_431() {
    let dir = temp_dir("header_limit");
    let server = Arc::new(
        Server::open(&dir)
            .unwrap()
            // Tiny header budget — a normal request line + a few headers
            // already blows it.
            .with_request_limits(48, 16 * 1024 * 1024),
    );
    let addr = spawn_server(Arc::clone(&server), 1);

    let mut req = String::from("GET /health HTTP/1.1\r\nHost: x\r\n");
    // Pad with a long header to overflow the 48-byte budget.
    req.push_str("X-Pad: ");
    req.push_str(&"a".repeat(200));
    req.push_str("\r\nConnection: close\r\n\r\n");
    let (status, raw) = raw_full(addr, req.as_bytes());
    assert_eq!(status, 431, "expected 431 headers too large; got:\n{raw}");
    assert!(raw.contains("headers_too_large"), "error code missing:\n{raw}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn read_timeout_releases_slot() {
    // A client that connects, sends a partial request, then stalls must
    // have its slot reclaimed once the read timeout elapses — the server
    // does not pin the connection forever.
    let dir = temp_dir("read_timeout");
    let server = Arc::new(
        Server::open(&dir)
            .unwrap()
            .with_max_connections(4)
            .with_timeouts(Duration::from_millis(300), Duration::from_secs(5)),
    );
    let (addr, handle) = spawn_server_forever(Arc::clone(&server));

    // Open a connection, send only the request line (no terminating blank
    // line), then stall. The server's read_line blocks and times out.
    let mut stalled = TcpStream::connect(addr).unwrap();
    stalled
        .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n")
        .unwrap();
    stalled.flush().unwrap();

    // Slot taken.
    let deadline = Instant::now() + Duration::from_secs(2);
    while server.in_flight() == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(server.in_flight(), 1, "stalled connection should hold a slot");

    // Within ~the read timeout, the slot is reclaimed even though we never
    // closed the socket from our side.
    let deadline = Instant::now() + Duration::from_secs(3);
    while server.in_flight() > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        server.in_flight(),
        0,
        "read timeout should have released the stalled connection's slot",
    );

    drop(stalled);
    handle.request();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn metrics_endpoint_exposes_expected_series() {
    let dir = temp_dir("metrics");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn_server(Arc::clone(&server), 3);

    // Drive a couple of requests so the counters are non-trivial.
    let _ = get(addr, "/health");
    let _ = get(addr, "/this/route/does/not/exist");

    let (status, raw) = raw_full(
        addr,
        b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 200, "got:\n{raw}");
    // Prometheus content type.
    assert!(
        raw.to_lowercase().contains("content-type: text/plain"),
        "metrics should be text/plain:\n{raw}",
    );
    // Body (after header block) carries the expected series.
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
    for series in [
        "ndb_requests_total",
        "ndb_responses_total",
        "ndb_connections_in_flight",
        "ndb_connections_rejected_total",
        "ndb_request_duration_seconds_sum",
        "ndb_request_duration_seconds_count",
        "ndb_bytes_read_total",
        "ndb_bytes_written_total",
    ] {
        assert!(body.contains(series), "missing series {series}:\n{body}");
    }
    // The /health request we made should appear as a labeled route series.
    assert!(
        body.contains("ndb_requests_total{route=\"/health\"}"),
        "missing /health route label:\n{body}",
    );
    // A response-status series for 200 must be present.
    assert!(
        body.contains("ndb_responses_total{status=\"200\"}"),
        "missing status=200 series:\n{body}",
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ready_returns_200_when_engine_usable() {
    let dir = temp_dir("ready_ok");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn_server(Arc::clone(&server), 1);
    let resp = get(addr, "/ready");
    assert_eq!(resp.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(body["status"], "ready");
    assert!(body["last_tx_id"].is_number(), "ready should report last_tx_id");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ready_returns_503_while_shutting_down() {
    // Once shutdown is requested, /ready flips to 503 (readiness) so an
    // orchestrator drains traffic, while /health stays 200 (liveness).
    //
    // The accept loop KEEPS accepting during the bounded drain window, so
    // we can probe /ready over the wire. To keep the window open long
    // enough (the loop returns the instant in-flight hits zero), we hold
    // one slow connection in-flight while we probe.
    let dir = temp_dir("ready_draining");
    let server = Arc::new(
        Server::open(&dir)
            .unwrap()
            .with_shutdown_drain_timeout(Duration::from_secs(3))
            // Long read timeout so the held connection stays in-flight.
            .with_timeouts(Duration::from_secs(5), Duration::from_secs(5)),
    );
    let (addr, handle) = spawn_server_forever(Arc::clone(&server));

    // Hold one connection in-flight (announce a body, never send it).
    let mut held = TcpStream::connect(addr).unwrap();
    held.write_all(
        b"POST /commit HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 64\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    held.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while server.in_flight() == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(server.in_flight() >= 1, "held connection should be in-flight");

    // /health stays 200 (liveness) even before shutdown.
    assert_eq!(get(addr, "/health").status, 200);

    // Flip the shutdown flag; the accept loop drains (still accepting) for
    // up to 3s — long enough for the /ready probe below.
    handle.request();
    let resp = get(addr, "/ready");
    assert_eq!(resp.status, 503, "ready must be 503 while shutting down");
    let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(body["status"], "not_ready");

    // /health still 200 during drain (liveness unaffected).
    assert_eq!(get(addr, "/health").status, 200);

    drop(held);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn graceful_shutdown_drains_and_serve_returns() {
    // Start a server on a worker thread via serve(); request shutdown and
    // assert the serve() call returns (the worker thread joins) within the
    // drain window, with in-flight at zero.
    let dir = temp_dir("graceful_shutdown");
    let server = Arc::new(
        Server::open(&dir)
            .unwrap()
            .with_shutdown_drain_timeout(Duration::from_secs(2)),
    );
    let bind = server.bind("127.0.0.1:0").unwrap();
    let addr = bind.local_addr().unwrap();
    let handle = server.shutdown_handle();
    drop(bind);

    let returned = Arc::new(AtomicBool::new(false));
    let returned_w = Arc::clone(&returned);
    let srv = Arc::clone(&server);
    let worker = thread::spawn(move || {
        let bind = srv.bind(addr).unwrap();
        let _ = bind.serve();
        returned_w.store(true, Ordering::SeqCst);
    });
    thread::sleep(Duration::from_millis(50));

    // Sanity: server answers before shutdown.
    assert_eq!(get(addr, "/health").status, 200);

    // Request graceful shutdown.
    let t0 = Instant::now();
    handle.request();

    // serve() should return promptly (no in-flight to drain).
    worker.join().expect("serve worker should join");
    assert!(returned.load(Ordering::SeqCst), "serve() must have returned");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "shutdown should be prompt with no in-flight; took {:?}",
        t0.elapsed(),
    );
    assert_eq!(server.in_flight(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn admin_shutdown_route_triggers_drain() {
    // POST /admin/shutdown sets the flag; the serve loop then returns.
    let dir = temp_dir("admin_shutdown");
    let server = Arc::new(Server::open(&dir).unwrap());
    let bind = server.bind("127.0.0.1:0").unwrap();
    let addr = bind.local_addr().unwrap();
    drop(bind);
    let srv = Arc::clone(&server);
    let worker = thread::spawn(move || {
        let bind = srv.bind(addr).unwrap();
        let _ = bind.serve();
    });
    thread::sleep(Duration::from_millis(50));

    let resp = post(addr, "/admin/shutdown", "{}");
    assert_eq!(resp.status, 202, "admin shutdown should return 202 Accepted");
    let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(body["status"], "shutting_down");

    assert!(server.is_shutting_down(), "flag should be set");
    worker.join().expect("serve should return after admin shutdown");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn admin_shutdown_requires_admin_capability() {
    // With a principals registry installed, /admin/shutdown is gated by
    // the Admin capability. A read-only principal gets 403; the flag stays
    // unset.
    let dir = temp_dir("admin_shutdown_authz");
    let mut principals = Principals::default();
    principals.principals.insert(
        "reader-token".to_string(),
        Principal {
            name: "reader".to_string(),
            capabilities: [Capability::Read].into_iter().collect(),
            entity_id: None,
        },
    );
    let server = Arc::new(Server::open(&dir).unwrap().with_principals(principals));
    let addr = spawn_server(Arc::clone(&server), 2);

    // Reader (no Admin) → 403.
    let req = b"POST /admin/shutdown HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer reader-token\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let (status, raw) = raw_full(addr, req);
    assert_eq!(status, 403, "reader should be forbidden; got:\n{raw}");
    assert!(!server.is_shutting_down(), "flag must stay unset on 403");

    // No token at all → 401.
    let req2 = b"POST /admin/shutdown HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let (status2, _raw2) = raw_full(addr, req2);
    assert_eq!(status2, 401, "missing token should be 401");
    assert!(!server.is_shutting_down(), "flag must stay unset on 401");

    std::fs::remove_dir_all(&dir).unwrap();
}
