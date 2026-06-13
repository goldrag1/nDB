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

    // An id present on NO shard is a clean not-found through the router
    // (hash-first hits the owning shard, scatter finds nothing → non-live).
    let absent = uuid::Uuid::now_v7().to_string();
    let (st, body) = get(router, &format!("/v1/read/{absent}"));
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_ne!(v["outcome"], "live", "absent id must be non-live");

    // Scatter-gather: /iter merges records from both shards (a@x and b@x).
    let (st, body) = get(router, "/v1/iter");
    assert_eq!(st, 200, "iter: {body}");
    assert!(body.contains("a@x"), "iter missing shard-0 record:\n{body}");
    assert!(body.contains("b@x"), "iter missing shard-1 record:\n{body}");

    std::fs::remove_dir_all(&dir0).ok();
    std::fs::remove_dir_all(&dir1).ok();
}

#[test]
fn router_commit_routes_entities_and_hyperedge_to_anchor() {
    let dir0 = temp_dir("c0");
    let dir1 = temp_dir("c1");
    let s0 = Arc::new(Server::open(&dir0).unwrap());
    let s1 = Arc::new(Server::open(&dir1).unwrap());
    let a0 = spawn_shard(Arc::clone(&s0));
    let a1 = spawn_shard(Arc::clone(&s1));
    let urls = vec![format!("http://{a0}"), format!("http://{a1}")];
    let shard_addr = [a0, a1];
    let map = ShardMap::new(urls.clone());
    let router = spawn_router(urls);

    // Commit two entities THROUGH the router; each must land on its owning shard.
    let ent_a = uuid::Uuid::now_v7().to_string();
    let ent_b = uuid::Uuid::now_v7().to_string();
    for (id, email) in [(&ent_a, "ea@x"), (&ent_b, "eb@x")] {
        let body = format!(
            "{{\"records\":[{{\"kind\":\"entity\",\"entity_id\":\"{id}\",\"type_id\":1,\"tx_id_assert\":0,\"tx_id_supersede\":\"active\",\"properties\":[{{\"prop_id\":10,\"value\":{{\"tag\":\"string\",\"value\":\"{email}\"}}}}]}}]}}"
        );
        let (st, b) = post(router, "/v1/commit", &body);
        assert_eq!(st, 200, "router commit entity {id}: {b}");
    }
    // Each entity is physically on its owning shard (read the shard directly).
    for id in [&ent_a, &ent_b] {
        let owner = map.shard_for_key(id);
        let (st, body) = get(shard_addr[owner], &format!("/v1/read/{id}"));
        assert_eq!(st, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["outcome"], "live",
            "entity {id} must be on its owning shard {owner}"
        );
    }

    // Commit a hyperedge THROUGH the router. Its anchor is ent_a, so it must
    // land on hash(ent_a)'s shard — NOT hash(hyperedge_id)'s shard.
    let he_id = uuid::Uuid::now_v7().to_string();
    let anchor_shard = map.shard_for_key(&ent_a);
    let body = format!(
        "{{\"records\":[{{\"kind\":\"hyper_edge\",\"hyperedge_id\":\"{he_id}\",\"type_id\":2,\"tx_id_assert\":0,\"tx_id_supersede\":\"active\",\"roles\":[{{\"role_id\":7,\"entity_id\":\"{ent_a}\"}},{{\"role_id\":8,\"entity_id\":\"{ent_b}\"}}],\"properties\":[]}}]}}"
    );
    let (st, b) = post(router, "/v1/commit", &body);
    assert_eq!(st, 200, "router commit hyperedge: {b}");

    // Physically on the anchor shard:
    let (st, body) = get(shard_addr[anchor_shard], &format!("/v1/read/{he_id}"));
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["outcome"], "live",
        "hyperedge must live on its anchor shard {anchor_shard}"
    );

    // And reachable via the router by its own id (hash-first + scatter-on-miss,
    // since hash(he_id) likely != anchor shard).
    let (st, body) = get(router, &format!("/v1/read/{he_id}"));
    assert_eq!(st, 200, "router read hyperedge: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["outcome"], "live",
        "router must find the anchor-placed hyperedge by id"
    );

    std::fs::remove_dir_all(&dir0).ok();
    std::fs::remove_dir_all(&dir1).ok();
}

#[test]
fn router_vector_search_merges_global_top_k() {
    use ndb_engine::PropertyId;

    let dir0 = temp_dir("v0");
    let dir1 = temp_dir("v1");
    let s0 = Arc::new(Server::open(&dir0).unwrap());
    let s1 = Arc::new(Server::open(&dir1).unwrap());
    // Register the vector property on both shards before any commits.
    for s in [&s0, &s1] {
        s.engine()
            .write()
            .unwrap()
            .register_vector_property(PropertyId::new(13));
    }
    let a0 = spawn_shard(Arc::clone(&s0));
    let a1 = spawn_shard(Arc::clone(&s1));
    let urls = vec![format!("http://{a0}"), format!("http://{a1}")];
    let router = spawn_router(urls);

    // Commit 6 entities THROUGH the router with 2-D vectors [d, 0] for d=1..=6.
    // The router scatters them across both shards by hash(entity_id).
    let mut id_for_d: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for d in 1..=6i64 {
        let id = uuid::Uuid::now_v7().to_string();
        id_for_d.insert(id.clone(), d);
        let body = format!(
            "{{\"records\":[{{\"kind\":\"entity\",\"entity_id\":\"{id}\",\"type_id\":1,\"tx_id_assert\":0,\"tx_id_supersede\":\"active\",\"properties\":[{{\"prop_id\":13,\"value\":{{\"tag\":\"vector\",\"value\":[{d}.0,0.0]}}}}]}}]}}"
        );
        let (st, b) = post(router, "/v1/commit", &body);
        assert_eq!(st, 200, "router commit vector entity d={d}: {b}");
    }

    // kNN through the router with k=3, query at the origin. L2Squared distance
    // of [d,0] from origin is d²: the global top-3 must be d=1,2,3 → 1,4,9.
    let q = "{\"type_id\":1,\"property_id\":13,\"query\":[0.0,0.0],\"k\":3,\"metric\":\"l2\"}";
    let (st, body) = post(router, "/v1/vector_search", q);
    assert_eq!(st, 200, "vector_search: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let hits = v["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 3, "global top-k must be exactly 3: {body}");

    // Ascending distance, and exactly d=1,2,3 (distances 1,4,9) regardless of
    // which shard each vector landed on — proves the cross-shard top-k merge.
    let dists: Vec<f64> = hits
        .iter()
        .map(|h| h["distance"].as_f64().unwrap())
        .collect();
    assert!(
        dists[0] <= dists[1] && dists[1] <= dists[2],
        "hits not ascending: {dists:?}"
    );
    let ds: Vec<i64> = hits
        .iter()
        .map(|h| id_for_d[h["entity_id"].as_str().unwrap()])
        .collect();
    assert_eq!(
        ds,
        vec![1, 2, 3],
        "global top-3 must be the 3 closest vectors, got d={ds:?}"
    );
    for (got, want) in dists.iter().zip([1.0, 4.0, 9.0]) {
        assert!(
            (got - want).abs() < 1e-3,
            "distance {got} != {want} (L2Squared)"
        );
    }

    std::fs::remove_dir_all(&dir0).ok();
    std::fs::remove_dir_all(&dir1).ok();
}

#[test]
fn router_traverse_finds_cross_shard_neighbors() {
    let dir0 = temp_dir("t0");
    let dir1 = temp_dir("t1");
    let s0 = Arc::new(Server::open(&dir0).unwrap());
    let s1 = Arc::new(Server::open(&dir1).unwrap());
    let a0 = spawn_shard(Arc::clone(&s0));
    let a1 = spawn_shard(Arc::clone(&s1));
    let urls = vec![format!("http://{a0}"), format!("http://{a1}")];
    let shard_addr = [a0, a1];
    let map = ShardMap::new(urls.clone());
    let router = spawn_router(urls);

    // A, B, and C chosen so C lands on a DIFFERENT shard than A.
    let ent_a = uuid::Uuid::now_v7().to_string();
    let ent_b = uuid::Uuid::now_v7().to_string();
    let mut ent_c = uuid::Uuid::now_v7().to_string();
    while map.shard_for_key(&ent_c) == map.shard_for_key(&ent_a) {
        ent_c = uuid::Uuid::now_v7().to_string();
    }
    let commit = |id: &str| {
        let body = format!(
            "{{\"records\":[{{\"kind\":\"entity\",\"entity_id\":\"{id}\",\"type_id\":1,\"tx_id_assert\":0,\"tx_id_supersede\":\"active\",\"properties\":[]}}]}}"
        );
        assert_eq!(post(router, "/v1/commit", &body).0, 200);
    };
    commit(&ent_a);
    commit(&ent_b);
    commit(&ent_c);

    // edge1 (A,B): anchor A → on hash(A)'s shard.
    // edge2 (C,A): anchor C → on hash(C)'s shard (≠ A's), with A a NON-anchor member.
    for (anchor_role, other_role, he) in [(&ent_a, &ent_b, "edge1"), (&ent_c, &ent_a, "edge2")] {
        let he_id = uuid::Uuid::now_v7().to_string();
        let body = format!(
            "{{\"records\":[{{\"kind\":\"hyper_edge\",\"hyperedge_id\":\"{he_id}\",\"type_id\":2,\"tx_id_assert\":0,\"tx_id_supersede\":\"active\",\"roles\":[{{\"role_id\":7,\"entity_id\":\"{anchor_role}\"}},{{\"role_id\":8,\"entity_id\":\"{other_role}\"}}],\"properties\":[]}}]}}"
        );
        assert_eq!(post(router, "/v1/commit", &body).0, 200, "commit {he}");
    }

    // 1-hop traverse from A through the router must reach BOTH B and C.
    let tq = format!("{{\"start\":\"{ent_a}\",\"hops\":[{{}}]}}");
    let (st, body) = post(router, "/v1/traverse", &tq);
    assert_eq!(st, 200, "router traverse: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let reached: Vec<&str> = v["entity_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x.as_str())
        .collect();
    assert!(
        reached.contains(&ent_b.as_str()),
        "must reach B (edge on A's shard): {reached:?}"
    );
    assert!(
        reached.contains(&ent_c.as_str()),
        "must reach C (edge on C's shard — needs scatter): {reached:?}"
    );

    // Discriminator: a single-shard traverse on A's shard alone MISSES C
    // (edge2 lives on C's shard), proving the router's cross-shard scatter is
    // what makes the neighbor set complete.
    let a_shard = map.shard_for_key(&ent_a);
    let (st, sbody) = post(shard_addr[a_shard], "/v1/traverse", &tq);
    assert_eq!(st, 200);
    let sv: serde_json::Value = serde_json::from_str(&sbody).unwrap();
    let sreached: Vec<&str> = sv["entity_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x.as_str())
        .collect();
    assert!(
        !sreached.contains(&ent_c.as_str()),
        "single shard must NOT see the off-shard edge: {sreached:?}"
    );

    std::fs::remove_dir_all(&dir0).ok();
    std::fs::remove_dir_all(&dir1).ok();
}
