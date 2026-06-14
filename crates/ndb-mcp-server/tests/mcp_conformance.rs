//! MCP protocol conformance suite — the "spec client" tests.
//!
//! Why this file exists: the production content-envelope bug
//! (`tools/call` returned the raw tool result instead of the spec-mandated
//! `result.content = [{type:"text", text}]`) was **invisible to `curl`** —
//! the HTTP status was 200 and the JSON parsed fine. It only broke when a
//! real, spec-compliant MCP client tried to read `result.content` and found
//! nothing. The existing unit tests in `lib.rs` assert our *implementation*
//! fields (`result.status`, `result.tx_id`, `result.records`) but never the
//! protocol *contract* a client depends on — so they were green through the
//! bug.
//!
//! This suite acts as a minimal MCP client: it issues each protocol method
//! exactly as a client would (id-correlated JSON-RPC 2.0 frames) and asserts
//! the shapes the **2024-11-05 MCP spec** mandates — `initialize` handshake,
//! `tools/list` JSON-Schema validity, the `tools/call` content envelope,
//! `resources/*`, `prompts/*`, and JSON-RPC error frames.
//!
//! The transport (HTTP/TLS framing) is covered separately by the unit tests
//! in `http.rs`; here we drive the public `handle_line` dispatch, which is the
//! exact code path every transport funnels through, so the assertions hold for
//! stdio and HTTP alike.
//!
//! Run explicitly: `cargo test -p ndb-mcp-server --test mcp_conformance`.

use std::sync::atomic::{AtomicI64, Ordering};

use ndb_mcp_server::McpServer;
use serde_json::{Value, json};

/// Monotonic request ids so each test can confirm the server echoes `id` back
/// (a real client correlates responses to requests by id).
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

fn temp_db() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ndb-mcp-conf-{}", uuid::Uuid::now_v7().simple()));
    p
}

/// A spec client's single round trip: build a JSON-RPC 2.0 request frame, send
/// it through the server's public dispatch, and apply the invariants that hold
/// for **every** response before handing the value back to the caller:
///   * `jsonrpc == "2.0"`
///   * the response echoes the request `id`
///   * exactly one of `result` / `error` is present (JSON-RPC 2.0 §5).
fn rpc(server: &McpServer, method: &str, params: Option<Value>) -> Value {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut req = json!({"jsonrpc": "2.0", "id": id, "method": method});
    if let Some(p) = params {
        req["params"] = p;
    }
    let resp = server.handle_line(&req.to_string());
    let v = serde_json::to_value(&resp).expect("response must serialize to JSON");

    assert_eq!(v["jsonrpc"], "2.0", "every frame is JSON-RPC 2.0: {v}");
    assert_eq!(v["id"], json!(id), "response must echo the request id: {v}");
    let has_result = v.get("result").is_some();
    let has_error = v.get("error").is_some();
    assert!(
        has_result ^ has_error,
        "exactly one of result/error per JSON-RPC 2.0: {v}"
    );
    v
}

// ---------------------------------------------------------------------------
// initialize handshake
// ---------------------------------------------------------------------------

#[test]
fn initialize_handshake_is_well_formed() {
    let dir = temp_db();
    let server = McpServer::open(&dir).unwrap();

    let v = rpc(&server, "initialize", None);
    let r = &v["result"];

    // protocolVersion: a non-empty string the client negotiates against.
    assert!(
        r["protocolVersion"].as_str().is_some_and(|s| !s.is_empty()),
        "initialize must return a protocolVersion string: {v}"
    );
    // capabilities: an object advertising tools/resources/prompts.
    assert!(
        r["capabilities"].is_object(),
        "initialize must return a capabilities object: {v}"
    );
    // serverInfo: name + version strings.
    assert!(
        r["serverInfo"]["name"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "serverInfo.name must be a non-empty string: {v}"
    );
    assert!(
        r["serverInfo"]["version"].as_str().is_some(),
        "serverInfo.version must be a string: {v}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// tools/list — JSON-Schema validity of every advertised tool
// ---------------------------------------------------------------------------

#[test]
fn tools_list_entries_are_valid_json_schema() {
    let dir = temp_db();
    let server = McpServer::open(&dir).unwrap();

    let v = rpc(&server, "tools/list", None);
    let tools = v["result"]["tools"]
        .as_array()
        .expect("tools/list must return a tools array");
    assert!(!tools.is_empty(), "tools list must be non-empty: {v}");

    for t in tools {
        let name = t["name"].as_str().unwrap_or("<unnamed>");
        // A client renders name + description and validates args against the
        // inputSchema, so all three must be present and well-typed.
        assert!(
            t["name"].as_str().is_some_and(|s| !s.is_empty()),
            "tool entry needs a name: {t}"
        );
        assert!(
            t["description"].as_str().is_some(),
            "tool {name} needs a description string"
        );
        let schema = &t["inputSchema"];
        assert_eq!(
            schema["type"], "object",
            "tool {name} inputSchema.type must be \"object\""
        );
        assert!(
            schema["properties"].is_object(),
            "tool {name} inputSchema.properties must be an object"
        );
        assert!(
            schema["required"].is_array(),
            "tool {name} inputSchema.required must be an array"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// tools/call content envelope — THE regression guard
// ---------------------------------------------------------------------------

/// This is the test that would have caught the production bug. The MCP spec
/// requires a successful `tools/call` to return
/// `result.content = [{type:"text", text:"…"}]` plus `result.isError: false`.
/// If `mcp_tool_result` is reverted to pass the raw tool output straight
/// through (the pre-fix behaviour), `result.content` is absent and this test
/// fails — exactly the signal that was missing when a real agent hit it.
#[test]
fn tools_call_returns_spec_content_envelope() {
    let dir = temp_db();
    let server = McpServer::open(&dir).unwrap();

    // ndb.health takes no arguments and always succeeds — the simplest probe.
    let v = rpc(&server, "tools/call", Some(json!({"name": "ndb.health"})));
    let r = &v["result"];

    // content MUST be a non-empty array of typed content blocks.
    let content = r["content"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/call result is missing the spec `content` array: {v}"));
    assert!(!content.is_empty(), "content array must be non-empty: {v}");

    // First block is a text block whose `text` is a string.
    assert_eq!(
        content[0]["type"], "text",
        "first content block must be type \"text\": {v}"
    );
    let text = content[0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("content[0].text must be a string: {v}"));

    // The text block carries the tool's serialized JSON — a client can parse
    // it back. Confirm it round-trips and preserves the structured payload.
    let parsed: Value = serde_json::from_str(text).expect("content[0].text must be parseable JSON");
    assert_eq!(
        parsed["status"], "ok",
        "envelope text must carry the result"
    );

    // isError MUST be present and false for a successful call.
    assert_eq!(
        r["isError"],
        json!(false),
        "successful tools/call must set isError:false: {v}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Every non-erroring tool in `tools/list` must produce the content envelope,
/// not just `ndb.health` — guards against a tool added later that bypasses
/// `mcp_tool_result`. We only call tools that are safe with empty/minimal args
/// and treat a JSON-RPC error frame as "this tool needs args" (still
/// spec-shaped), not an envelope violation.
#[test]
fn every_callable_tool_envelopes_or_errors_cleanly() {
    let dir = temp_db();
    let server = McpServer::open(&dir).unwrap();

    let list = rpc(&server, "tools/list", None);
    let names: Vec<String> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();

    for name in names {
        let v = rpc(&server, "tools/call", Some(json!({"name": name})));
        if v.get("result").is_some() {
            // A successful call MUST carry the envelope.
            assert!(
                v["result"]["content"]
                    .as_array()
                    .is_some_and(|c| !c.is_empty()),
                "tool {name} returned a result without a content envelope: {v}"
            );
            assert!(
                v["result"]["isError"].is_boolean(),
                "tool {name} result must set isError: {v}"
            );
        } else {
            // Missing/invalid args → a well-formed JSON-RPC error frame.
            assert!(
                v["error"]["code"].is_i64() && v["error"]["message"].is_string(),
                "tool {name} error frame must be {{code,message}}: {v}"
            );
        }
    }

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Error frames — JSON-RPC 2.0 reserved codes
// ---------------------------------------------------------------------------

#[test]
fn unknown_method_is_method_not_found() {
    let dir = temp_db();
    let server = McpServer::open(&dir).unwrap();

    let v = rpc(&server, "definitely/not/a/method", None);
    assert_eq!(
        v["error"]["code"].as_i64().unwrap(),
        -32601,
        "unknown method must be -32601 Method not found: {v}"
    );
    assert!(
        v["error"]["message"].is_string(),
        "error frame must carry a message: {v}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn malformed_json_is_parse_error() {
    let dir = temp_db();
    let server = McpServer::open(&dir).unwrap();

    // Drive a raw (un-parseable) line straight through, as the transport would.
    let resp = server.handle_line("this is not json {{{");
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(
        v["error"]["code"].as_i64().unwrap(),
        -32700,
        "malformed JSON must be -32700 Parse error: {v}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unknown_tool_is_a_clean_error_frame() {
    let dir = temp_db();
    let server = McpServer::open(&dir).unwrap();

    let v = rpc(
        &server,
        "tools/call",
        Some(json!({"name": "ndb.does_not_exist"})),
    );
    assert!(v.get("result").is_none(), "no result on tool error: {v}");
    assert!(
        v["error"]["code"].is_i64() && v["error"]["message"].is_string(),
        "unknown tool must yield a {{code,message}} error frame: {v}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// resources/list + resources/read
// ---------------------------------------------------------------------------

#[test]
fn resources_list_and_read_are_well_formed() {
    let dir = temp_db();
    let server = McpServer::open(&dir).unwrap();

    let list = rpc(&server, "resources/list", None);
    let resources = list["result"]["resources"]
        .as_array()
        .expect("resources/list must return a resources array");
    assert!(!resources.is_empty(), "resources list must be non-empty");
    for r in resources {
        assert!(r["uri"].as_str().is_some(), "resource needs a uri: {r}");
        assert!(r["name"].as_str().is_some(), "resource needs a name: {r}");
    }

    // Read one and confirm the spec `contents` array with a readable block.
    let uri = resources[0]["uri"].as_str().unwrap().to_owned();
    let read = rpc(&server, "resources/read", Some(json!({"uri": uri})));
    let contents = read["result"]["contents"]
        .as_array()
        .expect("resources/read must return a contents array");
    assert!(!contents.is_empty(), "contents must be non-empty: {read}");
    assert!(
        contents[0]["text"].as_str().is_some() || contents[0]["blob"].as_str().is_some(),
        "a resource content block needs text or blob: {read}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// prompts/list + prompts/get
// ---------------------------------------------------------------------------

#[test]
fn prompts_list_and_get_are_well_formed() {
    let dir = temp_db();
    let server = McpServer::open(&dir).unwrap();

    let list = rpc(&server, "prompts/list", None);
    let prompts = list["result"]["prompts"]
        .as_array()
        .expect("prompts/list must return a prompts array");
    assert!(!prompts.is_empty(), "prompts list must be non-empty");
    let name = prompts[0]["name"]
        .as_str()
        .expect("prompt needs a name")
        .to_owned();

    let got = rpc(
        &server,
        "prompts/get",
        Some(json!({"name": name, "arguments": {"uuid": "abc"}})),
    );
    let messages = got["result"]["messages"]
        .as_array()
        .expect("prompts/get must return a messages array");
    assert!(!messages.is_empty(), "messages must be non-empty: {got}");
    assert!(
        messages[0]["role"].as_str().is_some(),
        "a prompt message needs a role: {got}"
    );
    assert!(
        messages[0]["content"]["text"].as_str().is_some(),
        "a prompt message needs content.text: {got}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
