//! ndb-agent — a tiny "coding agent" that GROWS its memory in nDB, live.
//!
//! Every tick it "answers a question", then remembers it as a new N-ary fact:
//! an Observation entity linked (role 6) to the source File entities it found
//! relevant (role 2). The 3D viz polls nDB and shows the graph densify in real
//! time. Swap this loop for a real LLM agent calling the same MCP write tools.
//!
//!   ndb-agent 127.0.0.1:9000 <WRITE_TOKEN> [interval_secs]
//!
//! Types:  6 Observation   Props: 13 message  16 ts   Roles: 2 touches  6 observation
//! Edge:  102 AgentObservation (observation + the files it recalled)

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

mod embed;
use embed::embed16;

fn post(host_port: &str, token: &str, payload: &Value) -> Value {
    let body = serde_json::to_vec(payload).unwrap();
    let mut s = TcpStream::connect(host_port).expect("connect mcp");
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if !token.is_empty() {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(&body).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let text = String::from_utf8_lossy(&resp);
    let start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    serde_json::from_str(text[start..].trim()).unwrap_or(Value::Null)
}

fn call(hp: &str, tok: &str, name: &str, args: Value) -> Value {
    post(hp, tok, &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .get("result").cloned().unwrap_or(Value::Null)
}

fn now_ts() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0).to_string()
}

fn main() {
    // Read from args first, else environment (robust under systemd — ExecStart
    // ${VAR} expansion of EnvironmentFile vars is unreliable; env is not).
    let mut a = std::env::args().skip(1);
    let hp = a
        .next()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("NDB_MCP_ADDR").ok())
        .unwrap_or_else(|| "127.0.0.1:9000".into());
    let token = a
        .next()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("NDB_WRITE_TOKEN").ok())
        .unwrap_or_default();
    let interval: u64 = a
        .next()
        .and_then(|s| s.parse().ok())
        .or_else(|| std::env::var("NDB_AGENT_INTERVAL").ok().and_then(|s| s.parse().ok()))
        .unwrap_or(15);

    // Learn the current files (uuid + path) so observations link to real records.
    let recs = call(&hp, &token, "ndb.iter", json!({"limit":500}));
    let mut files: Vec<(String, String)> = Vec::new(); // (uuid, path)
    if let Some(arr) = recs.get("records").and_then(|r| r.as_array()) {
        for r in arr {
            if r.get("kind").and_then(|k| k.as_str()) == Some("entity")
                && r.get("type_id").and_then(serde_json::Value::as_u64) == Some(2)
            {
                let uuid = r.get("entity_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let path = r.get("properties").and_then(|p| p.as_array()).and_then(|ps| {
                    ps.iter().find(|p| p.get("prop_id").and_then(serde_json::Value::as_u64) == Some(12))
                        .and_then(|p| p.get("value")).and_then(|v| v.get("value")).and_then(|v| v.as_str())
                }).unwrap_or("").to_string();
                if !uuid.is_empty() { files.push((uuid, path)); }
            }
        }
    }
    eprintln!("ndb-agent: learned {} files; growing memory every {interval}s -> {hp}", files.len());

    // (question, path substrings the agent considers relevant)
    let qa: [(&str, &[&str]); 8] = [
        ("Where is TLS terminated?", &["http.rs", "ndb-server/src/lib.rs", "ndb-edge"]),
        ("How does the MCP transport authenticate writes?", &["http.rs", "main.rs"]),
        ("Where is the Pingora edge configured?", &["ndb-edge/src/main.rs", "ndb-edge.service"]),
        ("How is the demo data seeded?", &["seed.rs"]),
        ("What serves the web UI?", &["static_server.rs", "agent-memory.html"]),
        ("How does the Rust client speak https?", &["client/src/lib.rs"]),
        ("Where do the systemd units live?", &["ndb-edge.service", "units"]),
        ("How is a Let's Encrypt cert obtained?", &["obtain-certs.sh", "certsh", "README"]),
    ];

    let mut i: usize = 0;
    loop {
        let (q, hints) = qa[i % qa.len()];
        let ts = now_ts();
        let obs = call(&hp, &token, "ndb.commit_entity",
            json!({"type_id":6, "properties":[
                {"prop_id":13,"value":{"tag":"string","value":format!("Answered: {q}")}},
                {"prop_id":16,"value":{"tag":"string","value":ts}},
                {"prop_id":20,"value":{"tag":"vector","value":embed16(q)}}
            ]}));
        let obs_id = obs.get("entity_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if obs_id.is_empty() {
            eprintln!("ndb-agent: write blocked or failed (check WRITE_TOKEN) — resp: {obs}");
            sleep(Duration::from_secs(interval));
            continue;
        }
        // link the observation to the files it found relevant -> one N-ary fact
        let mut roles = vec![json!({"role_id":6,"entity_id":obs_id})];
        let mut linked = 0;
        for (uuid, path) in &files {
            if hints.iter().any(|h| path.contains(h)) {
                roles.push(json!({"role_id":2,"entity_id":uuid}));
                linked += 1;
            }
        }
        if linked == 0 { // fall back to first two files so the fact is still n-ary
            for (uuid, _) in files.iter().take(2) { roles.push(json!({"role_id":2,"entity_id":uuid})); linked += 1; }
        }
        call(&hp, &token, "ndb.commit_hyperedge",
            json!({"type_id":102, "roles":roles, "properties":[{"prop_id":13,"value":{"tag":"string","value":q}}]}));
        eprintln!("ndb-agent: remembered \"{q}\" -> {linked} files");
        i += 1;
        sleep(Duration::from_secs(interval));
    }
}
