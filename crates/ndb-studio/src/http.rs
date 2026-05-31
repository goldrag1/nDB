//! Local HTTP API + embedded web UI.
//!
//! A small hand-rolled HTTP/1.1 server (the same shape as `ndb-server` and the
//! langgraph explorer — raw `std::net`, thread-per-connection, `serde_json`),
//! deliberately kept as the *single* boundary the frontend talks to so a later
//! Tauri shell can swap `fetch` for `invoke` against the same routes.
//!
//! Auth: a session cookie (`ndb_session`) issued by `/api/login`. Reads need
//! any authenticated role; writes (`/api/commit`) need editor or admin; user
//! administration needs admin. The only unauthenticated routes are the UI,
//! `/api/health`, `/api/login`, and `/api/me`.
//!
//! Routes:
//! - `GET  /`               → the embedded single-file UI
//! - `GET  /api/health`     → liveness + head tx (public)
//! - `GET  /api/me`         → current session (public; reports anonymous)
//! - `POST /api/login`      → `{username,password}` → sets the session cookie
//! - `POST /api/logout`     → clears the session
//! - `GET  /api/catalog|table|record|history|pivot|graph` → reads (any role)
//! - `POST /api/query`      → run a read-only query (any role)
//! - `POST /api/commit`     → create / set / delete (editor/admin)
//! - `GET  /api/replication/status|since` → leader stream (admin)
//! - `POST /api/replication/apply|pull`   → follower ingest / pull (admin)
//! - `GET  /api/users`      → list accounts (admin)
//! - `POST /api/users`      → `{username,password,role}` create (admin)
//! - `POST /api/users/delete` → `{username}` (admin)

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use serde_json::{Value as J, json};
use uuid::Uuid;

use crate::identity::{self, Role, Session};
use crate::jsonval::from_json;
use crate::store::{Store, StoreError, TableQuery};

const INDEX_HTML: &str = include_str!("../web/index.html");
const COOKIE: &str = "ndb_session";

/// Everything a request handler needs: the engine-backed store and the
/// process-local session map.
pub struct AppState {
    /// The data store (the only thing that touches the engine).
    pub store: Arc<Store>,
    /// In-memory token → session map.
    pub sessions: identity::Sessions,
}

impl AppState {
    /// Build state around an opened store.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self { store, sessions: identity::Sessions::new() }
    }
}

/// Bind a listener so the caller can read the chosen port before serving.
///
/// # Errors
/// Propagates the bind error.
pub fn bind(addr: &str) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr)
}

/// Accept connections forever, dispatching each on its own thread.
///
/// # Errors
/// Propagates a fatal accept error.
pub fn run(listener: &TcpListener, state: &Arc<AppState>) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = Arc::clone(state);
        std::thread::spawn(move || {
            let _ = handle(&state, stream);
        });
    }
    Ok(())
}

fn handle(state: &AppState, mut stream: TcpStream) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_mins(1)));
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    // Drain headers, capturing Content-Length and the session cookie.
    let mut content_length = 0usize;
    let mut token: Option<String> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        if h == "\r\n" || h == "\n" || h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("cookie") {
                token = cookie_value(v.trim(), COOKIE);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let qp = parse_query(query);

    if method == "GET" && path == "/" {
        return write_html(&mut stream, INDEX_HTML);
    }
    if method == "GET" && path == "/favicon.ico" {
        return stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }

    let resp = dispatch(state, &method, path, &qp, &body, token.as_deref());
    write_resp(&mut stream, &resp)
}

/// A response: status, JSON body, and an optional `Set-Cookie`.
struct Resp {
    status: u16,
    body: J,
    set_cookie: Option<String>,
}

impl Resp {
    fn ok(body: J) -> Self {
        Self { status: 200, body, set_cookie: None }
    }
    fn code(status: u16, body: J) -> Self {
        Self { status, body, set_cookie: None }
    }
    fn fail(status: u16, code: &str, message: &str) -> Self {
        Self { status, body: err(code, message), set_cookie: None }
    }
}

#[allow(clippy::too_many_lines)]
fn dispatch(
    state: &AppState,
    method: &str,
    path: &str,
    qp: &Query,
    body: &[u8],
    token: Option<&str>,
) -> Resp {
    let store = &state.store;
    let session = token.and_then(|t| state.sessions.lookup(t));

    match (method, path) {
        // ---- public ----
        ("GET", "/api/health") => Resp::ok(json!({ "status": "ok", "head": store.head() })),
        ("GET", "/api/me") => match &session {
            Some(s) => Resp::ok(json!({ "authed": true, "user": s.username, "role": s.role.as_str() })),
            None => Resp::ok(json!({ "authed": false })),
        },
        ("POST", "/api/login") => login(state, body),
        ("POST", "/api/logout") => {
            if let Some(t) = token {
                state.sessions.revoke(t);
            }
            Resp { status: 200, body: json!({ "ok": true }), set_cookie: Some(clear_cookie()) }
        }

        // ---- reads (any authenticated role) ----
        ("GET", "/api/catalog") => guard_read(session.as_ref(), || Resp::ok(store.catalog(qp.u64("as_of")))),
        ("GET", "/api/table") => guard_read(session.as_ref(), || {
            let Some(kind) = qp.u64("kind") else {
                return Resp::fail(400, "bad_request", "missing kind");
            };
            let tq = TableQuery {
                as_of: qp.u64("as_of"),
                limit: usize::try_from(qp.u64("limit").unwrap_or(50)).unwrap_or(50),
                offset: usize::try_from(qp.u64("offset").unwrap_or(0)).unwrap_or(0),
                sort: qp.get("sort").filter(|s| !s.is_empty()).map(decode_uri),
                desc: qp.get("dir") == Some("desc"),
                q: qp.get("q").filter(|s| !s.is_empty()).map(decode_uri),
                // per-column filters arrive as f_<prop>=<substr>
                filters: qp.filters(),
            };
            #[allow(clippy::cast_possible_truncation)]
            Resp::ok(store.table(kind as u32, &tq))
        }),
        ("GET", "/api/record") => guard_read(session.as_ref(), || {
            let Some(id) = qp.get("id").and_then(|s| Uuid::parse_str(s).ok()) else {
                return Resp::fail(400, "bad_request", "missing or invalid id");
            };
            Resp::ok(store.record(id, qp.u64("as_of")))
        }),
        ("GET", "/api/history") => guard_read(session.as_ref(), || {
            let Some(id) = qp.get("id").and_then(|s| Uuid::parse_str(s).ok()) else {
                return Resp::fail(400, "bad_request", "missing or invalid id");
            };
            Resp::ok(store.history(id, qp.get("property")))
        }),
        ("GET", "/api/pivot") => guard_read(session.as_ref(), || {
            let Some(kind) = qp.u64("kind") else {
                return Resp::fail(400, "bad_request", "missing kind");
            };
            let (Some(row), Some(col)) = (qp.get("row"), qp.get("col")) else {
                return Resp::fail(400, "bad_request", "missing row or col property");
            };
            let agg = qp.get("agg").unwrap_or("count");
            #[allow(clippy::cast_possible_truncation)]
            Resp::ok(store.pivot(kind as u32, row, col, agg, qp.get("value"), qp.u64("as_of")))
        }),
        ("GET", "/api/graph") => guard_read(session.as_ref(), || {
            let limit = usize::try_from(qp.u64("limit").unwrap_or(300)).unwrap_or(300);
            Resp::ok(store.graph(qp.u64("as_of"), limit))
        }),
        ("GET", "/api/hyperedges") => guard_read(session.as_ref(), || {
            let Some(kind) = qp.u64("kind") else {
                return Resp::fail(400, "bad_request", "missing kind");
            };
            let limit = usize::try_from(qp.u64("limit").unwrap_or(500)).unwrap_or(500);
            #[allow(clippy::cast_possible_truncation)]
            Resp::ok(store.hyperedges(kind as u32, qp.u64("as_of"), limit))
        }),
        ("GET", "/api/similar") => guard_read(session.as_ref(), || {
            let Some(id) = qp.get("id").and_then(|s| Uuid::parse_str(s).ok()) else {
                return Resp::fail(400, "bad_request", "missing or invalid id");
            };
            let Some(property) = qp.get("property") else {
                return Resp::fail(400, "bad_request", "missing property");
            };
            let k = usize::try_from(qp.u64("k").unwrap_or(10)).unwrap_or(10);
            Resp::ok(store.find_similar(id, property, k))
        }),
        ("GET", "/api/schema") => guard_read(session.as_ref(), || Resp::ok(store.schema())),
        ("GET", "/api/diff") => guard_read(session.as_ref(), || {
            let (Some(from), Some(to)) = (qp.u64("from"), qp.u64("to")) else {
                return Resp::fail(400, "bad_request", "missing from/to tx");
            };
            Resp::ok(store.diff(from, to))
        }),
        ("POST", "/api/query") => guard_read(session.as_ref(), || run_query(store, body)),

        // ---- writes (editor / admin) ----
        ("POST", "/api/commit") => match writer(session.as_ref()) {
            Ok(author) => commit(store, body, &author),
            Err(r) => r,
        },

        // ---- user administration (admin) ----
        ("GET", "/api/users") => match admin(session.as_ref()) {
            Ok(()) => {
                let users: Vec<J> = store.list_users().into_iter()
                    .map(|(u, r)| json!({ "username": u, "role": r })).collect();
                Resp::ok(json!({ "users": users }))
            }
            Err(r) => r,
        },
        ("POST", "/api/users") => match admin(session.as_ref()) {
            Ok(()) => create_user(store, body),
            Err(r) => r,
        },
        ("POST", "/api/users/delete") => match admin(session.as_ref()) {
            Ok(()) => delete_user(store, body, session.as_ref()),
            Err(r) => r,
        },

        // ---- replication / node admin (admin) ----
        ("GET", "/api/replication/status") => match admin(session.as_ref()) {
            Ok(()) => Resp::ok(store.replication_status()),
            Err(r) => r,
        },
        ("GET", "/api/replication/since") => match admin(session.as_ref()) {
            Ok(()) => {
                let seq = qp.u64("seq").unwrap_or(0);
                let after = qp.u64("after").unwrap_or(0);
                Resp::ok(store.serve_replication(seq, after))
            }
            Err(r) => r,
        },
        ("POST", "/api/replication/apply") => match admin(session.as_ref()) {
            Ok(()) => apply_batch(store, body),
            Err(r) => r,
        },
        ("POST", "/api/replication/pull") => match admin(session.as_ref()) {
            Ok(()) => replication_pull(store, body),
            Err(r) => r,
        },

        _ => Resp::fail(404, "not_found", "unknown endpoint"),
    }
}

// ---- auth helpers ------------------------------------------------------

fn guard_read(session: Option<&Session>, f: impl FnOnce() -> Resp) -> Resp {
    if session.is_some() {
        f()
    } else {
        Resp::fail(401, "unauthorized", "login required")
    }
}

/// Require a writer (editor/admin); returns the author username on success.
fn writer(session: Option<&Session>) -> Result<String, Resp> {
    match session {
        None => Err(Resp::fail(401, "unauthorized", "login required")),
        Some(s) if s.role.can_write() => Ok(s.username.clone()),
        Some(_) => Err(Resp::fail(403, "forbidden", "editor role required to write")),
    }
}

fn admin(session: Option<&Session>) -> Result<(), Resp> {
    match session {
        None => Err(Resp::fail(401, "unauthorized", "login required")),
        Some(s) if s.role.is_admin() => Ok(()),
        Some(_) => Err(Resp::fail(403, "forbidden", "admin role required")),
    }
}

fn apply_batch(store: &Store, body: &[u8]) -> Resp {
    let Ok(req) = serde_json::from_slice::<J>(body) else {
        return Resp::fail(400, "bad_request", "invalid JSON body");
    };
    let b64 = req.get("records_b64").and_then(J::as_str).unwrap_or("");
    match store.ingest_replicated_b64(b64) {
        Ok(head) => Resp::ok(json!({ "ok": true, "head": head })),
        Err(e) => Resp::code(e.status(), json!({ "error": { "code": e.code(), "message": e.message() } })),
    }
}

/// Follower pull: stream the peer leader's WAL delta and ingest it, advancing
/// the cursor (a faithful reimplementation of `replication::poll_once` over the
/// HTTP transport, so network I/O happens outside the engine lock).
fn replication_pull(store: &Store, body: &[u8]) -> Resp {
    let Ok(req) = serde_json::from_slice::<J>(body) else {
        return Resp::fail(400, "bad_request", "invalid JSON body");
    };
    let peer = req.get("peer").and_then(J::as_str).unwrap_or("").trim_end_matches('/');
    let token = req.get("token").and_then(J::as_str).unwrap_or("");
    if peer.is_empty() || token.is_empty() {
        return Resp::fail(400, "bad_request", "peer URL and peer session token required");
    }

    // Start from the leader's active WAL segment, offset 0.
    let status = match http_get_json(peer, "/api/replication/status", token) {
        Ok(j) => j,
        Err(e) => return Resp::fail(502, "peer_unreachable", &e),
    };
    let mut seq = status["wal_seq"].as_u64().unwrap_or(0);
    let mut off = 0u64;
    let mut applied = 0u64;

    for _ in 0..100_000 {
        let path = format!("/api/replication/since?seq={seq}&after={off}");
        let b = match http_get_json(peer, &path, token) {
            Ok(j) => j,
            Err(e) => return Resp::fail(502, "peer_unreachable", &e),
        };
        if b.get("error").is_some() {
            return Resp::code(502, json!({ "error": { "code": "peer_error", "message": b["error"].clone() } }));
        }
        if !b["available"].as_bool().unwrap_or(false) {
            return Resp::fail(409, "rotated", "leader rotated past our position — re-bootstrap from a base backup");
        }
        let n = b["count"].as_u64().unwrap_or(0);
        if n > 0 {
            if let Err(e) = store.ingest_replicated_b64(b["records_b64"].as_str().unwrap_or("")) {
                return Resp::code(e.status(), json!({ "error": { "code": e.code(), "message": e.message() } }));
            }
            applied += n;
        }
        off = b["next_offset"].as_u64().unwrap_or(off);
        if n == 0 {
            if b["segment_sealed"].as_bool() == Some(true)
                && let Some(next) = b["next_wal_seq"].as_u64()
            {
                seq = next;
                off = 0;
                continue;
            }
            break; // caught up
        }
    }
    Resp::ok(json!({ "ok": true, "applied": applied, "head": store.head() }))
}

/// A minimal HTTP/1.1 GET that sends the session cookie and parses a JSON body.
/// Loopback / LAN, plaintext, `Connection: close` (no TLS, no chunked) — the
/// shape our own server speaks. Used only for follower→leader pulls.
fn http_get_json(peer: &str, path_and_query: &str, token: &str) -> Result<J, String> {
    let hostport = peer
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let mut stream = TcpStream::connect(hostport).map_err(|e| format!("connect {hostport}: {e}"))?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(15)));
    let req = format!(
        "GET {path_and_query} HTTP/1.1\r\nHost: {hostport}\r\nCookie: {COOKIE}={token}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| e.to_string())?;

    let sep = buf.windows(4).position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "malformed peer response (no header terminator)".to_string())?;
    let head = &buf[..sep];
    let bodytext = &buf[sep + 4..];
    let status_line = head.split(|&b| b == b'\n').next().unwrap_or(b"");
    let status: u16 = std::str::from_utf8(status_line).ok()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(format!("peer returned HTTP {status}"));
    }
    serde_json::from_slice(bodytext).map_err(|e| format!("peer JSON: {e}"))
}

fn run_query(store: &Store, body: &[u8]) -> Resp {
    let Ok(req) = serde_json::from_slice::<J>(body) else {
        return Resp::fail(400, "bad_request", "invalid JSON body");
    };
    let text = req.get("query").and_then(J::as_str).unwrap_or("");
    if text.trim().is_empty() {
        return Resp::fail(400, "bad_request", "empty query");
    }
    match store.query(text) {
        Ok(result) => Resp::ok(result),
        Err(envelope) => Resp::code(400, envelope),
    }
}

fn login(state: &AppState, body: &[u8]) -> Resp {
    let Ok(req) = serde_json::from_slice::<J>(body) else {
        return Resp::fail(400, "bad_request", "invalid JSON body");
    };
    let username = req.get("username").and_then(J::as_str).unwrap_or("");
    let password = req.get("password").and_then(J::as_str).unwrap_or("");
    if let Some((_, pwhash, role_str)) = state.store.find_user(username)
        && identity::verify_password(password, &pwhash)
    {
        let role = Role::parse(&role_str);
        let tok = state.sessions.issue(username, role);
        return Resp {
            status: 200,
            body: json!({ "user": username, "role": role.as_str() }),
            set_cookie: Some(session_cookie(&tok)),
        };
    }
    Resp::fail(401, "bad_credentials", "invalid username or password")
}

fn create_user(store: &Store, body: &[u8]) -> Resp {
    let Ok(req) = serde_json::from_slice::<J>(body) else {
        return Resp::fail(400, "bad_request", "invalid JSON body");
    };
    let username = req.get("username").and_then(J::as_str).unwrap_or("").trim();
    let password = req.get("password").and_then(J::as_str).unwrap_or("");
    let role_str = req.get("role").and_then(J::as_str).unwrap_or("viewer");
    if username.is_empty() || password.is_empty() {
        return Resp::fail(400, "bad_request", "username and password required");
    }
    if !matches!(role_str, "viewer" | "editor" | "admin") {
        return Resp::fail(400, "bad_request", "role must be viewer, editor or admin");
    }
    let hash = identity::hash_password(password);
    match store.create_user(username, &hash, role_str) {
        Ok(tx) => Resp::ok(json!({ "ok": true, "tx": tx, "username": username, "role": role_str })),
        Err(e) => Resp::code(e.status(), json!({ "error": { "code": e.code(), "message": e.message() } })),
    }
}

fn delete_user(store: &Store, body: &[u8], session: Option<&Session>) -> Resp {
    let Ok(req) = serde_json::from_slice::<J>(body) else {
        return Resp::fail(400, "bad_request", "invalid JSON body");
    };
    let username = req.get("username").and_then(J::as_str).unwrap_or("");
    if session.is_some_and(|s| s.username == username) {
        return Resp::fail(400, "bad_request", "cannot delete your own account");
    }
    match store.delete_user(username) {
        Ok(tx) => Resp::ok(json!({ "ok": true, "tx": tx })),
        Err(e) => Resp::code(e.status(), json!({ "error": { "code": e.code(), "message": e.message() } })),
    }
}

fn commit(store: &Store, body: &[u8], author: &str) -> Resp {
    let Ok(req) = serde_json::from_slice::<J>(body) else {
        return Resp::fail(400, "bad_request", "invalid JSON body");
    };
    let op = req.get("op").and_then(J::as_str).unwrap_or("");
    match op {
        "create" => {
            let kind = req.get("kind").and_then(J::as_str).unwrap_or("");
            if kind.is_empty() {
                return Resp::fail(400, "bad_request", "missing kind");
            }
            let mut props = Vec::new();
            if let Some(arr) = req.get("properties").and_then(J::as_array) {
                for p in arr {
                    let Some(name) = p.get("name").and_then(J::as_str) else {
                        return Resp::fail(400, "bad_request", "property missing name");
                    };
                    let value = match from_json(p.get("value").unwrap_or(&J::Null)) {
                        Ok(v) => v,
                        Err(m) => return Resp::fail(400, "bad_value", &m),
                    };
                    props.push((name.to_string(), value));
                }
            }
            finish(store.create(kind, &props, Some(author)))
        }
        "set" => {
            let Some(id) = req.get("id").and_then(J::as_str).and_then(|s| Uuid::parse_str(s).ok())
            else {
                return Resp::fail(400, "bad_request", "missing or invalid id");
            };
            let Some(property) = req.get("property").and_then(J::as_str) else {
                return Resp::fail(400, "bad_request", "missing property");
            };
            let value = match from_json(req.get("value").unwrap_or(&J::Null)) {
                Ok(v) => v,
                Err(m) => return Resp::fail(400, "bad_value", &m),
            };
            finish(store.set(id, property, &value, Some(author)))
        }
        "delete" => {
            let Some(id) = req.get("id").and_then(J::as_str).and_then(|s| Uuid::parse_str(s).ok())
            else {
                return Resp::fail(400, "bad_request", "missing or invalid id");
            };
            finish(store.delete(id))
        }
        "create_edge" => {
            let kind = req.get("kind").and_then(J::as_str).unwrap_or("");
            if kind.is_empty() {
                return Resp::fail(400, "bad_request", "missing edge kind");
            }
            let mut roles = Vec::new();
            if let Some(arr) = req.get("roles").and_then(J::as_array) {
                for r in arr {
                    let role = r.get("role").and_then(J::as_str).unwrap_or("");
                    let Some(target) = r.get("ref").and_then(J::as_str).and_then(|s| Uuid::parse_str(s).ok())
                    else {
                        return Resp::fail(400, "bad_request", "role missing valid 'ref' entity id");
                    };
                    if role.is_empty() {
                        return Resp::fail(400, "bad_request", "role missing name");
                    }
                    roles.push((role.to_string(), target));
                }
            }
            finish(store.create_hyperedge(kind, &roles))
        }
        "register_vector" => {
            let prop = req.get("property").and_then(J::as_str).unwrap_or("");
            if prop.is_empty() {
                return Resp::fail(400, "bad_request", "missing property");
            }
            match store.register_vector(prop) {
                Ok(()) => Resp::ok(json!({ "ok": true })),
                Err(e) => Resp::code(e.status(), json!({ "error": { "code": e.code(), "message": e.message() } })),
            }
        }
        _ => Resp::fail(400, "bad_request", "unknown op"),
    }
}

fn finish(result: Result<u64, StoreError>) -> Resp {
    match result {
        Ok(tx) => Resp::ok(json!({ "ok": true, "tx": tx })),
        Err(e) => Resp::code(e.status(), json!({ "error": { "code": e.code(), "message": e.message() } })),
    }
}

fn err(code: &str, message: &str) -> J {
    json!({ "error": { "code": code, "message": message } })
}

// ---- tiny HTTP plumbing ------------------------------------------------

fn session_cookie(token: &str) -> String {
    format!("{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/")
}
fn clear_cookie() -> String {
    format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

/// Extract `name`'s value from a `Cookie:` header line.
fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

struct Query(HashMap<String, String>);

impl Query {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    fn u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|s| s.parse().ok())
    }
    /// Per-column table filters supplied as `f_<prop>=<substr>`.
    fn filters(&self) -> Vec<(String, String)> {
        self.0
            .iter()
            .filter_map(|(k, v)| k.strip_prefix("f_").map(|p| (decode_uri(p), decode_uri(v))))
            .collect()
    }
}

/// Minimal percent-decode (`%XX`) + `+`→space, for filter/search/sort values.
fn decode_uri(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(h) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    out.push(h);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => { out.push(b' '); i += 1; }
            c => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(query: &str) -> Query {
    let map = query
        .split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect();
    Query(map)
}

fn write_resp(stream: &mut TcpStream, resp: &Resp) -> std::io::Result<()> {
    let payload = serde_json::to_vec(&resp.body).unwrap_or_default();
    let mut header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {len}\r\nConnection: close\r\n",
        status = resp.status,
        reason = reason(resp.status),
        len = payload.len(),
    );
    if let Some(c) = &resp.set_cookie {
        header.push_str("Set-Cookie: ");
        header.push_str(c);
        header.push_str("\r\n");
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes())?;
    stream.write_all(&payload)
}

fn write_html(stream: &mut TcpStream, html: &str) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        len = html.len(),
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(html.as_bytes())
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Error",
    }
}
