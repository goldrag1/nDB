//! Local HTTP API + embedded web UI.
//!
//! A small hand-rolled HTTP/1.1 server (the same shape as `ndb-server` and the
//! langgraph explorer — raw `std::net`, thread-per-connection, `serde_json`),
//! deliberately kept as the *single* boundary the frontend talks to so a later
//! Tauri shell can swap `fetch` for `invoke` against the same routes.
//!
//! Routes (v1):
//! - `GET  /`               → the embedded single-file UI
//! - `GET  /api/health`     → liveness + head tx
//! - `GET  /api/catalog`    → kinds + properties (`?as_of=`)
//! - `GET  /api/table`      → one kind as a table (`?kind=&as_of=&limit=`)
//! - `GET  /api/record`     → one record's full property list (`?id=&as_of=`)
//! - `POST /api/commit`     → create / set / delete (body: `{op, ...}`)

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use serde_json::{Value as J, json};
use uuid::Uuid;

use crate::jsonval::from_json;
use crate::store::{Store, StoreError};

const INDEX_HTML: &str = include_str!("../web/index.html");

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
pub fn run(listener: &TcpListener, store: &Arc<Store>) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let store = Arc::clone(store);
        std::thread::spawn(move || {
            let _ = handle(&store, stream);
        });
    }
    Ok(())
}

fn handle(store: &Store, mut stream: TcpStream) -> std::io::Result<()> {
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

    // Drain headers, capturing Content-Length.
    let mut content_length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        if h == "\r\n" || h == "\n" || h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            content_length = v.trim().parse().unwrap_or(0);
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

    let (status, payload) = dispatch(store, &method, path, &qp, &body);
    write_json(&mut stream, status, &payload)
}

fn dispatch(store: &Store, method: &str, path: &str, qp: &Query, body: &[u8]) -> (u16, J) {
    match (method, path) {
        ("GET", "/api/health") => (200, json!({ "status": "ok", "head": store.head() })),
        ("GET", "/api/catalog") => (200, store.catalog(qp.u64("as_of"))),
        ("GET", "/api/table") => {
            let Some(kind) = qp.u64("kind") else {
                return (400, err("bad_request", "missing kind"));
            };
            let limit = usize::try_from(qp.u64("limit").unwrap_or(1000)).unwrap_or(1000);
            #[allow(clippy::cast_possible_truncation)]
            (200, store.table(kind as u32, qp.u64("as_of"), limit))
        }
        ("GET", "/api/record") => {
            let Some(id) = qp.get("id").and_then(|s| Uuid::parse_str(s).ok()) else {
                return (400, err("bad_request", "missing or invalid id"));
            };
            (200, store.record(id, qp.u64("as_of")))
        }
        ("POST", "/api/commit") => commit(store, body),
        _ => (404, err("not_found", "unknown endpoint")),
    }
}

fn commit(store: &Store, body: &[u8]) -> (u16, J) {
    let Ok(req) = serde_json::from_slice::<J>(body) else {
        return (400, err("bad_request", "invalid JSON body"));
    };
    let op = req.get("op").and_then(J::as_str).unwrap_or("");
    match op {
        "create" => {
            let kind = req.get("kind").and_then(J::as_str).unwrap_or("");
            if kind.is_empty() {
                return (400, err("bad_request", "missing kind"));
            }
            let mut props = Vec::new();
            if let Some(arr) = req.get("properties").and_then(J::as_array) {
                for p in arr {
                    let Some(name) = p.get("name").and_then(J::as_str) else {
                        return (400, err("bad_request", "property missing name"));
                    };
                    let value = match from_json(p.get("value").unwrap_or(&J::Null)) {
                        Ok(v) => v,
                        Err(m) => return (400, err("bad_value", &m)),
                    };
                    props.push((name.to_string(), value));
                }
            }
            finish(store.create(kind, &props))
        }
        "set" => {
            let Some(id) = req.get("id").and_then(J::as_str).and_then(|s| Uuid::parse_str(s).ok())
            else {
                return (400, err("bad_request", "missing or invalid id"));
            };
            let Some(property) = req.get("property").and_then(J::as_str) else {
                return (400, err("bad_request", "missing property"));
            };
            let value = match from_json(req.get("value").unwrap_or(&J::Null)) {
                Ok(v) => v,
                Err(m) => return (400, err("bad_value", &m)),
            };
            finish(store.set(id, property, &value))
        }
        "delete" => {
            let Some(id) = req.get("id").and_then(J::as_str).and_then(|s| Uuid::parse_str(s).ok())
            else {
                return (400, err("bad_request", "missing or invalid id"));
            };
            finish(store.delete(id))
        }
        _ => (400, err("bad_request", "unknown op")),
    }
}

fn finish(result: Result<u64, StoreError>) -> (u16, J) {
    match result {
        Ok(tx) => (200, json!({ "ok": true, "tx": tx })),
        Err(e) => (e.status(), json!({ "error": { "code": e.code(), "message": e.message() } })),
    }
}

fn err(code: &str, message: &str) -> J {
    json!({ "error": { "code": code, "message": message } })
}

// ---- tiny HTTP plumbing ------------------------------------------------

struct Query(HashMap<String, String>);

impl Query {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    fn u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|s| s.parse().ok())
    }
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

fn write_json(stream: &mut TcpStream, status: u16, body: &J) -> std::io::Result<()> {
    let payload = serde_json::to_vec(body).unwrap_or_default();
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        reason = reason(status),
        len = payload.len(),
    );
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
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Error",
    }
}
