//! ndb-seed — seed the "agent memory" graph into nDB over the MCP HTTP transport.
//!
//! Rust-native (std + serde_json). Run it ON the box against the plain local MCP
//! port so no TLS/token plumbing is needed:
//!
//!   ndb-seed 127.0.0.1:9000 <BEARER_TOKEN_or_empty>
//!
//! The data is a coding agent's memory of building this stack: people, files,
//! issues, PRs, and commits — each commit stored as ONE N-ary hyperedge
//! (commit + author + files-touched + issue-closed). Commit order = tx order,
//! so the UI's time-travel slider replays the project's growth.

use std::io::{Read, Write};
use std::net::TcpStream;

use serde_json::{json, Value};

fn post(host_port: &str, token: &str, payload: &Value) -> Value {
    let body = serde_json::to_vec(payload).expect("serialize");
    let mut stream = TcpStream::connect(host_port).expect("connect to MCP");
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if !token.is_empty() {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(&body).unwrap();
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).unwrap();
    let text = String::from_utf8_lossy(&resp);
    let start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let v: Value = serde_json::from_str(text[start..].trim())
        .unwrap_or_else(|e| panic!("bad JSON response: {e}\n{}", &text[start..]));
    if let Some(err) = v.get("error") {
        panic!("MCP error: {err}");
    }
    v["result"].clone()
}

fn s(v: &str) -> Value {
    json!({"tag": "string", "value": v})
}

struct Mcp {
    host_port: String,
    token: String,
}

impl Mcp {
    fn call(&self, name: &str, args: Value) -> Value {
        post(
            &self.host_port,
            &self.token,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}),
        )
    }
    fn entity(&self, type_id: u32, props: &[(u32, &str)]) -> String {
        let properties: Vec<Value> = props.iter().map(|(p, v)| json!({"prop_id": p, "value": s(v)})).collect();
        self.call("ndb.commit_entity", json!({"type_id": type_id, "properties": properties}))["entity_id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn edge(&self, type_id: u32, roles: &[(u32, &str)], props: &[(u32, &str)]) {
        let r: Vec<Value> = roles.iter().map(|(rid, eid)| json!({"role_id": rid, "entity_id": eid})).collect();
        let p: Vec<Value> = props.iter().map(|(pid, v)| json!({"prop_id": pid, "value": s(v)})).collect();
        self.call("ndb.commit_hyperedge", json!({"type_id": type_id, "roles": r, "properties": p}));
    }
}

fn pos(defs: &[(&str, &str, &str)], key: &str) -> usize {
    defs.iter().position(|d| d.0 == key).expect("known key")
}

fn main() {
    let mut args = std::env::args().skip(1);
    let host_port = args.next().unwrap_or_else(|| "127.0.0.1:9000".into());
    let token = args.next().unwrap_or_default();
    let m = Mcp { host_port: host_port.clone(), token };
    eprintln!("seeding agent memory -> {host_port}");

    // Types: 1 Person 2 File 3 Commit 4 Issue 5 PullRequest
    // Props: 10 name 12 path 13 message 14 title 15 sha 16 ts 21 status 22 meta
    // Roles: 1 author 2 touches 3 closes 4 commit 5 reviewer
    // Edges: 100 CommitEvent  101 PullRequest

    let people_def: [(&str, &str, &str); 4] = [
        ("long", "Long", "maintainer"),
        ("claude", "Claude (agent)", "coding agent"),
        ("mai", "Mai", "backend dev"),
        ("tuan", "Tuan", "infra/SRE"),
    ];
    let files_def: [(&str, &str, &str); 13] = [
        ("http", "crates/ndb-mcp-server/src/http.rs", "rust"),
        ("mcplib", "crates/ndb-mcp-server/src/lib.rs", "rust"),
        ("mcpmain", "crates/ndb-mcp-server/src/main.rs", "rust"),
        ("mcptoml", "crates/ndb-mcp-server/Cargo.toml", "toml"),
        ("srvlib", "crates/ndb-server/src/lib.rs", "rust"),
        ("edge", "deploy/sovereign/ndb-edge/src/main.rs", "rust"),
        ("edgetoml", "deploy/sovereign/ndb-edge/Cargo.toml", "toml"),
        ("units", "deploy/sovereign/systemd/ndb-edge.service", "systemd"),
        ("certsh", "deploy/sovereign/scripts/obtain-certs.sh", "bash"),
        ("readme", "deploy/sovereign/README.md", "markdown"),
        ("client", "crates/ndb-client-rust/src/lib.rs", "rust"),
        ("seedrs", "deploy/sovereign/demo/src/seed.rs", "rust"),
        ("statichtml", "deploy/sovereign/demo/agent-memory.html", "html"),
    ];
    let issues_def: [(&str, &str, &str); 7] = [
        ("remote", "Remote agents can't reach MCP (stdio only)", "closed"),
        ("tls", "MCP server can't terminate TLS", "closed"),
        ("edge", "Need an all-Rust TLS edge (no tunnel)", "closed"),
        ("cohere", "/v1 and /mcp back separate DBs", "closed"),
        ("clidoc", "Docs claim Rust CLI can't do TLS (stale)", "closed"),
        ("pyseed", "Seed tool was Python — make it Rust-native", "closed"),
        ("demo", "Ship an agent-memory demo devs can poke", "open"),
    ];

    let people: Vec<String> = people_def.iter().map(|&(_, n, meta)| m.entity(1, &[(10, n), (22, meta)])).collect();
    let files: Vec<String> = files_def.iter().map(|&(_, p, l)| m.entity(2, &[(12, p), (22, l)])).collect();
    let issues: Vec<String> = issues_def.iter().map(|&(_, t, st)| m.entity(4, &[(14, t), (21, st)])).collect();
    eprintln!("  {} people, {} files, {} issues", people.len(), files.len(), issues.len());

    // (author, message, [file keys], issue key or "", sha)
    let commits: [(&str, &str, &[&str], &str, &str); 15] = [
        ("claude", "feat(mcp): Streamable HTTP transport (POST /mcp, /health, bearer, CORS)", &["http", "mcplib", "mcpmain"], "remote", "3cf1bd5"),
        ("claude", "test(mcp): 4 transport tests — 401, 405, init, tools/list", &["http"], "remote", "a1b2c3d"),
        ("long", "review: confirm handle_line reuse — no dispatch fork needed", &["mcplib"], "", "0099aa1"),
        ("claude", "feat(mcp): native rustls TLS termination (--tls-cert/--tls-key)", &["http", "mcptoml", "mcpmain"], "tls", "d4e5f60"),
        ("claude", "refactor(mcp): generic handle_connection over Read+Write (TLS reuse)", &["http"], "tls", "77ab120"),
        ("mai", "chore: mirror ndb-server rustls loader shape for consistency", &["http", "srvlib"], "tls", "55cd221"),
        ("claude", "feat(deploy): Pingora TLS edge (ndb-edge) — path-route /mcp + /v1", &["edge", "edgetoml"], "edge", "9e0f1a2"),
        ("tuan", "fix(deploy): pin pingora 0.8.1 — 0.4.0 fails on 2026 http crate", &["edgetoml"], "edge", "b3c4d5e"),
        ("claude", "feat(deploy): systemd units (cgroup-capped) + Let's Encrypt script", &["units", "certsh"], "edge", "6f7a8b9"),
        ("long", "docs: fix stale 'Rust CLI can't do TLS' caveat — client speaks https", &["readme", "client"], "clidoc", "1c2d3e4"),
        ("tuan", "chore(infra): clear Frappe off longsv1, install Pingora+nDB stack", &["units"], "", "8a9b0c1"),
        ("claude", "feat(infra): single-store coherence — ndb-mcp as the memory writer", &["mcpmain", "readme"], "cohere", "2e3f4a5"),
        ("claude", "refactor(demo): seed in Rust, not Python — all-Rust tooling", &["seedrs"], "pyseed", "aa11bb2"),
        ("mai", "feat(demo): agent-memory web UI served from the Rust edge", &["statichtml"], "demo", "f0a1b2c"),
        ("claude", "feat(demo): seed the agent's memory of building this stack", &["statichtml", "seedrs"], "demo", "c3d4e5f"),
    ];

    let base_ts: i64 = 1_718_330_000;
    let mut commit_ids: Vec<String> = Vec::new();
    for (i, &(author, msg, fkeys, ikey, sha)) in commits.iter().enumerate() {
        let cts = (base_ts + (i as i64) * 3600).to_string();
        let cid = m.entity(3, &[(13, msg), (15, sha), (16, cts.as_str())]);
        // ONE N-ary fact: commit + author + each file + (issue) — a single record.
        let mut roles: Vec<(u32, &str)> = vec![(4, cid.as_str()), (1, people[pos(&people_def, author)].as_str())];
        for &fk in fkeys {
            roles.push((2, files[pos(&files_def, fk)].as_str()));
        }
        if !ikey.is_empty() {
            roles.push((3, issues[pos(&issues_def, ikey)].as_str()));
        }
        m.edge(100, &roles, &[(13, msg), (15, sha), (16, cts.as_str())]);
        commit_ids.push(cid);
    }
    eprintln!("  {} commits, {} N-ary commit facts", commit_ids.len(), commit_ids.len());

    // one PR record: author + reviewer + first 12 commits + 3 issues, all in one hyperedge
    let _pr = m.entity(5, &[(14, "PR #2: network MCP transport + sovereign deploy"), (21, "merged"), (15, "PR-2")]);
    let mut pr_roles: Vec<(u32, &str)> = vec![
        (1, people[pos(&people_def, "claude")].as_str()),
        (5, people[pos(&people_def, "long")].as_str()),
    ];
    for cid in commit_ids.iter().take(12) {
        pr_roles.push((4, cid.as_str()));
    }
    for ik in ["remote", "tls", "edge"] {
        pr_roles.push((3, issues[pos(&issues_def, ik)].as_str()));
    }
    m.edge(101, &pr_roles, &[(14, "PR #2"), (21, "merged")]);
    eprintln!("  1 pull request (N-ary: author + reviewer + 12 commits + 3 issues)");

    let stats = post(
        &host_port,
        &m.token,
        &json!({"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"ndb://stats"}}),
    );
    eprintln!("stats: {}", stats["contents"][0]["text"].as_str().unwrap_or("?"));
    eprintln!("done.");
}
