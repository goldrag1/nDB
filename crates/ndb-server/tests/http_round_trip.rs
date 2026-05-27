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
use ndb_server::Server;

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
        let mut e = e.lock().unwrap();
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
        let mut e = e.lock().unwrap();
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
    let req = "GET /iter HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\nConnection: close\r\n\r\n";
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
        let mut e = e.lock().unwrap();
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
fn malformed_uuid_in_read_returns_400() {
    let dir = temp_dir("bad_uuid");
    let server = Arc::new(Server::open(&dir).unwrap());
    let addr = spawn_server(Arc::clone(&server), 1);
    let resp = get(addr, "/read/not-a-uuid");
    assert_eq!(resp.status, 400);
    std::fs::remove_dir_all(&dir).unwrap();
}
