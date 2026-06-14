//! Unified single-engine, dual-surface test.
//!
//! Before this, `/v1` (ndb-server) and `/mcp` (ndb-mcp-server) were separate
//! processes over separate single-writer databases — a write through one was
//! invisible to the other. `Server::from_shared` / `McpServer::from_shared` let
//! both surfaces share ONE `Arc<RwLock<Engine>>` in a single process (the
//! `--v1-bind` unified launch).
//!
//! Acceptance: a write committed via `/mcp` (a `tools/call ndb.commit_entity`)
//! is instantly readable via `/v1` (`GET /v1/read/:uuid`) on the same engine.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use ndb_mcp_server::McpServer;
use ndb_server::Server;
use serde_json::Value;

fn temp_db() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ndb-mcp-unified-{}", uuid::Uuid::now_v7().simple()));
    p
}

/// Drive one HTTP request through the v1 server's public `handle_connection`
/// and return the raw response. `serve_one` handles exactly one request then
/// returns; `handle_connection` consumes (and drops) the stream on return,
/// which closes the socket and unblocks the client's `read_to_string`.
fn v1_request(server: &Server, raw: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        resp
    });
    let (stream, _) = listener.accept().unwrap();
    server.handle_connection(stream).unwrap();
    client.join().unwrap()
}

fn v1_get(server: &Server, path: &str) -> String {
    v1_request(
        server,
        format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    )
}

#[test]
fn mcp_write_is_instantly_readable_via_v1_on_one_engine() {
    let dir = temp_db();

    // One engine, two surfaces: the MCP server owns the engine; the v1 server
    // is built from a clone of the SAME shared handle.
    let mcp = McpServer::open(&dir).unwrap();
    let v1 = Server::from_shared(mcp.engine());

    // Sanity: a not-yet-written UUID reads `outcome:"missing"` on /v1 (so a
    // later `outcome:"live"` proves it was *this* write, not a stale record).
    let bogus = uuid::Uuid::now_v7();
    let miss = v1_get(&v1, &format!("/v1/read/{bogus}"));
    assert!(
        miss.contains("\"missing\"") && !miss.contains("\"live\""),
        "an unwritten uuid must read outcome:missing on /v1: {miss}"
    );

    // Write via /mcp: tools/call ndb.commit_entity.
    let commit = serde_json::to_value(mcp.handle_line(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"ndb.commit_entity",
            "arguments":{
                "type_id":1,
                "properties":[{"prop_id":10,"value":{"tag":"string","value":"shared-engine"}}]
            }
        }}"#,
    ))
    .unwrap();
    let uuid = commit["result"]["entity_id"]
        .as_str()
        .unwrap_or_else(|| panic!("commit_entity must return entity_id: {commit}"))
        .to_owned();
    let tx_id = commit["result"]["tx_id"].as_u64().unwrap();
    assert!(tx_id > 0, "commit must advance the tx: {commit}");

    // Read it back via /v1 — the OTHER surface, same engine, no flush/sync.
    let resp = v1_get(&v1, &format!("/v1/read/{uuid}"));
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "the /mcp-committed entity must read 200 on /v1: {resp}"
    );

    // The body parses, reports the entity live, and round-trips both the uuid
    // and the property written via /mcp through the shared engine.
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    let parsed: Value = serde_json::from_str(body.trim())
        .unwrap_or_else(|_| panic!("v1 body must be JSON: {body}"));
    assert_eq!(
        parsed["outcome"], "live",
        "the /mcp write must read live on /v1: {parsed}"
    );
    assert!(
        parsed.to_string().contains(&uuid),
        "the /v1 read must carry the committed uuid {uuid}: {parsed}"
    );
    assert!(
        parsed.to_string().contains("shared-engine"),
        "the property written via /mcp must be visible via /v1: {parsed}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
