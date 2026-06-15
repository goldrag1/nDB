//! ndb-embed — a tiny local embedding service (real model, sovereign).
//!
//! Loads BAAI/bge-small-en-v1.5 (384-dim, ONNX via fastembed) once, then serves
//! plain HTTP on localhost so ndb-mcp can auto-embed text on write and embed
//! queries for semantic search. No cloud, no API key.
//!
//!   POST /embed  {"texts":["a","b"]}  ->  {"dim":384,"vectors":[[...],[...]]}
//!   POST /embed  {"text":"a"}         ->  {"dim":384,"vectors":[[...]]}
//!   GET  /health -> {"status":"ok","dim":384,"model":"bge-small-en-v1.5"}
//!
//! Env: NDB_EMBED_BIND (default 127.0.0.1:8090). First run downloads the model.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use serde_json::{json, Value};

fn write_json(stream: &mut TcpStream, code: u16, reason: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn handle(model: &TextEmbedding, stream: &mut TcpStream) {
    let mut reader = match stream.try_clone() {
        Ok(s) => BufReader::new(s),
        Err(_) => return,
    };
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let mut content_length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        if h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if method == "GET" && path == "/health" {
        write_json(stream, 200, "OK", r#"{"status":"ok","dim":384,"model":"bge-small-en-v1.5"}"#);
        return;
    }
    if method != "POST" {
        write_json(stream, 404, "Not Found", r#"{"error":"POST /embed"}"#);
        return;
    }
    let mut body = vec![0u8; content_length.min(4 * 1024 * 1024)];
    if reader.read_exact(&mut body).is_err() {
        write_json(stream, 400, "Bad Request", r#"{"error":"short body"}"#);
        return;
    }
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            write_json(stream, 400, "Bad Request", &format!("{{\"error\":\"json: {e}\"}}"));
            return;
        }
    };
    let texts: Vec<String> = if let Some(arr) = v.get("texts").and_then(|t| t.as_array()) {
        arr.iter().filter_map(|x| x.as_str().map(String::from)).collect()
    } else if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
        vec![t.to_string()]
    } else {
        write_json(stream, 400, "Bad Request", r#"{"error":"need text or texts"}"#);
        return;
    };
    match model.embed(texts, None) {
        Ok(vecs) => {
            let dim = vecs.first().map_or(0, Vec::len);
            let out = json!({"dim": dim, "vectors": vecs});
            write_json(stream, 200, "OK", &out.to_string());
        }
        Err(e) => write_json(stream, 500, "Error", &format!("{{\"error\":\"embed: {e}\"}}")),
    }
}

fn main() {
    let bind = std::env::var("NDB_EMBED_BIND").unwrap_or_else(|_| "127.0.0.1:8090".into());
    eprintln!("ndb-embed: loading bge-small-en-v1.5 (first run downloads the model)…");
    let model = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::BGESmallENV15))
        .expect("init embedding model");
    let listener = TcpListener::bind(&bind).expect("bind");
    eprintln!("ndb-embed: ready on http://{bind} (384-dim, POST /embed)");
    for conn in listener.incoming() {
        if let Ok(mut stream) = conn {
            handle(&model, &mut stream);
        }
    }
}
