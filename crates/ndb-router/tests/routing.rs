//! Router integration: 2 real `ndb-server` shards behind an `ndb-router`.
//! Verifies point reads route by `hash(id)%N` (to the OWNING shard, not a
//! broadcast) and `/iter` scatter-gathers across shards.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ndb_router::{Router, ShardMap};
use ndb_server::Server;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ndb-router-{tag}-{}",
        uuid::Uuid::now_v7().simple()
    ));
    p
}

/// Spawn an `ndb-server` shard on an ephemeral port; return its address.
fn spawn_shard(server: Arc<Server>) -> SocketAddr {
    let bind = server.bind("127.0.0.1:0").unwrap();
    let addr = bind.local_addr().unwrap();
    let srv = Arc::clone(&server);
    thread::spawn(move || {
        let bind = srv.bind(addr).unwrap();
        let _ = bind.serve_n(10_000);
    });
    drop(bind);
    thread::sleep(Duration::from_millis(50));
    addr
}

/// Spawn the router over the given shard URLs; return its address.
fn spawn_router(shard_urls: Vec<String>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Arc::new(Router::new(ShardMap::new(shard_urls)));
    thread::spawn(move || {
        let _ = router.serve_listener(listener);
    });
    thread::sleep(Duration::from_millis(50));
    addr
}

fn raw(addr: SocketAddr, request: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    s.flush().unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, body.to_string())
}

fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    raw(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    )
}

fn post(addr: SocketAddr, path: &str, json: &str) -> (u16, String) {
    raw(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
            json.len()
        ),
    )
}

fn commit_entity(addr: SocketAddr, id: &str, email: &str) {
    let body = format!(
        "{{\"records\":[{{\"kind\":\"entity\",\"entity_id\":\"{id}\",\"type_id\":1,\"tx_id_assert\":0,\"tx_id_supersede\":\"active\",\"properties\":[{{\"prop_id\":10,\"value\":{{\"tag\":\"string\",\"value\":\"{email}\"}}}}]}}]}}"
    );
    let (status, b) = post(addr, "/v1/commit", &body);
    assert_eq!(status, 200, "shard commit failed: {b}");
}

#[test]
fn router_point_routes_and_scatter_gathers() {
    // Two shards.
    let dir0 = temp_dir("s0");
    let dir1 = temp_dir("s1");
    let s0 = Arc::new(Server::open(&dir0).unwrap());
    let s1 = Arc::new(Server::open(&dir1).unwrap());
    let a0 = spawn_shard(Arc::clone(&s0));
    let a1 = spawn_shard(Arc::clone(&s1));
    let urls = vec![format!("http://{a0}"), format!("http://{a1}")];
    let shard_addr = [a0, a1];

    let map = ShardMap::new(urls.clone());
    let router = spawn_router(urls);

    // Health reports both shards.
    let (hs, hb) = get(router, "/v1/health");
    assert_eq!(hs, 200);
    assert!(hb.contains("\"shards\":2"), "health: {hb}");

    // Two ids; commit each to the shard the router will route it to.
    let id_a = uuid::Uuid::now_v7().to_string();
    let id_b = uuid::Uuid::now_v7().to_string();
    commit_entity(shard_addr[map.shard_for_key(&id_a)], &id_a, "a@x");
    commit_entity(shard_addr[map.shard_for_key(&id_b)], &id_b, "b@x");

    // Point reads route to the owning shard and find the records.
    for id in [&id_a, &id_b] {
        let (st, body) = get(router, &format!("/v1/read/{id}"));
        assert_eq!(st, 200, "read {id}: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["outcome"], "live",
            "router should route {id} to its owning shard"
        );
    }

    // Discriminating case: an id committed to the WRONG (non-owning) shard is
    // NOT found through the router — proving it routes by hash, not broadcast.
    let id_c = uuid::Uuid::now_v7().to_string();
    let owning = map.shard_for_key(&id_c);
    let wrong = 1 - owning;
    commit_entity(shard_addr[wrong], &id_c, "c@x");
    let (st, body) = get(router, &format!("/v1/read/{id_c}"));
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_ne!(
        v["outcome"], "live",
        "router must read only the owning shard, not the wrong one"
    );

    // Scatter-gather: /iter merges records from both shards (a@x and b@x).
    let (st, body) = get(router, "/v1/iter");
    assert_eq!(st, 200, "iter: {body}");
    assert!(body.contains("a@x"), "iter missing shard-0 record:\n{body}");
    assert!(body.contains("b@x"), "iter missing shard-1 record:\n{body}");

    std::fs::remove_dir_all(&dir0).ok();
    std::fs::remove_dir_all(&dir1).ok();
}
