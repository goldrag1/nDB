//! ndb-static — a tiny std-only static file server for the demo web UI.
//!
//! Rust-native replacement for `python3 -m http.server`. Plain HTTP on
//! localhost; the Pingora edge terminates TLS in front and routes non-/mcp
//! paths here. Thread-per-connection, GET only, path-traversal-safe.
//!
//!   NDB_STATIC_BIND  listen addr   (default 127.0.0.1:8080)
//!   NDB_STATIC_ROOT  web root      (default /var/www/ndb)   "/" -> index.html

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn send(stream: &mut TcpStream, code: u16, reason: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn handle(stream: &mut TcpStream, root: &Path) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");
    if method != "GET" {
        send(stream, 405, "Method Not Allowed", "text/plain", b"GET only");
        return;
    }
    // strip query, decode nothing fancy, map "/" -> index.html
    let mut path = raw_path.split('?').next().unwrap_or("/").to_string();
    if path == "/" {
        path = "/index.html".into();
    }
    // path-traversal guard: reject any "..", build under root
    if path.contains("..") {
        send(stream, 403, "Forbidden", "text/plain", b"no");
        return;
    }
    let rel = path.trim_start_matches('/');
    let mut full = PathBuf::from(root);
    full.push(rel);
    match std::fs::read(&full) {
        Ok(bytes) => send(stream, 200, "OK", content_type(&full), &bytes),
        Err(_) => send(stream, 404, "Not Found", "text/plain", b"not found"),
    }
}

fn main() {
    let bind = env_or("NDB_STATIC_BIND", "127.0.0.1:8080");
    let root = PathBuf::from(env_or("NDB_STATIC_ROOT", "/var/www/ndb"));
    let listener = TcpListener::bind(&bind).expect("bind static server");
    eprintln!("ndb-static: serving {} on http://{bind}", root.display());
    for conn in listener.incoming() {
        if let Ok(mut stream) = conn {
            let root = root.clone();
            std::thread::spawn(move || handle(&mut stream, &root));
        }
    }
}
