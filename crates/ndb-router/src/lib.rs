//! nDB sharding coordinator.
//!
//! A stateless router that speaks the same `/v1` wire protocol to clients and
//! fans requests out across N single-writer shards (each a plain `ndb-server`).
//! Clients and the MCP server point at the router exactly as they would a
//! single server — the SDK is unchanged.
//!
//! Routing model (see `docs/superpowers/specs/2026-06-14-ndb-scale-sharding-design.md`):
//! - **Shard key:** `hash(entity_id) % N` (D1). UUIDv7 ids hash uniformly.
//! - **Point reads** (`/v1/read/:id`) route to the one owning shard.
//! - **Scans** (`/v1/iter`) scatter to every shard and merge.
//!
//! This is the read-path increment. Mutation routing (commit-splitting,
//! hyperedge anchor placement per D2) and kNN merge are subsequent increments.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

use ndb_client::Client;

/// The static shard map: shard index → shard server base URL.
#[derive(Debug, Clone)]
pub struct ShardMap {
    shards: Vec<String>,
}

impl ShardMap {
    /// Build a shard map from shard base URLs (e.g. `http://shard-0:8742`).
    ///
    /// # Panics
    /// If `shards` is empty — a router needs at least one shard.
    #[must_use]
    pub fn new(shards: Vec<String>) -> Self {
        assert!(!shards.is_empty(), "shard map needs at least one shard");
        Self { shards }
    }

    /// Number of shards.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shards.len()
    }

    /// Always false (the map is never empty — see [`new`](Self::new)).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }

    /// The shard base URLs, in index order.
    #[must_use]
    pub fn urls(&self) -> &[String] {
        &self.shards
    }

    /// The shard index that owns `key` (an entity/hyperedge id). Stable across
    /// processes: the key is normalised (hyphens dropped, lower-cased) before
    /// hashing, so `read` and a future `commit` agree regardless of id casing.
    #[must_use]
    pub fn shard_for_key(&self, key: &str) -> usize {
        let h = fnv1a_64(normalize_id(key).as_bytes());
        usize::try_from(h % self.shards.len() as u64).unwrap_or(0)
    }

    /// The shard base URL owning `key`.
    #[must_use]
    pub fn url_for_key(&self, key: &str) -> &str {
        &self.shards[self.shard_for_key(key)]
    }
}

/// Normalise a UUID-ish id for stable hashing: drop hyphens, lower-case.
fn normalize_id(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// FNV-1a (64-bit) — a small, stable, well-distributed hash with no deps.
/// (`DefaultHasher` is explicitly not stable across releases, so we can't use
/// it for a routing key that must be reproducible.)
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The router: a shard map + the serve loop.
pub struct Router {
    map: ShardMap,
}

impl Router {
    /// Build a router over the given shard map.
    #[must_use]
    pub fn new(map: ShardMap) -> Self {
        Self { map }
    }

    /// The shard map (for tests / introspection).
    #[must_use]
    pub fn map(&self) -> &ShardMap {
        &self.map
    }

    /// Serve forever on `addr`, thread-per-connection. Returns only on a fatal
    /// listener error.
    ///
    /// # Errors
    /// If binding `addr` fails.
    pub fn serve(self: std::sync::Arc<Self>, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        self.serve_listener(listener)
    }

    /// Serve forever on an already-bound listener. Lets a caller (e.g. a test)
    /// bind an ephemeral port and read the address before serving.
    ///
    /// # Errors
    /// Propagates a fatal accept error (individual bad connections are skipped).
    pub fn serve_listener(
        self: std::sync::Arc<Self>,
        listener: TcpListener,
    ) -> std::io::Result<()> {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let me = std::sync::Arc::clone(&self);
            std::thread::spawn(move || {
                let _ = me.handle(stream);
            });
        }
        Ok(())
    }

    /// Handle one request (one request per connection; `Connection: close`).
    fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        // Drain headers (read path needs no body).
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
                break;
            }
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let raw_path = parts.next().unwrap_or("");
        let path = canonicalize_v1(raw_path.split('?').next().unwrap_or(""));

        let (status, ctype, body) = self.route(method, path);
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
            reason = reason(status),
            len = body.len(),
        )?;
        stream.write_all(&body)?;
        stream.flush()
    }

    /// Map a (method, canonical-path) to a response.
    fn route(&self, method: &str, path: &str) -> (u16, &'static str, Vec<u8>) {
        match (method, path) {
            ("GET", "/health") => (
                200,
                "application/json",
                format!("{{\"status\":\"ok\",\"shards\":{}}}", self.map.len()).into_bytes(),
            ),
            ("GET", p) if p.starts_with("/read/") => {
                let id = &p["/read/".len()..];
                self.route_read(id)
            }
            ("GET", "/iter") => self.route_iter(),
            _ => error_body(
                501,
                "not_implemented",
                "router read-path increment: only GET /v1/health, /v1/read/:id, /v1/iter are routed yet",
            ),
        }
    }

    /// Point read: route to the owning shard and return its response verbatim.
    fn route_read(&self, id: &str) -> (u16, &'static str, Vec<u8>) {
        if id.is_empty() {
            return error_body(400, "bad_request", "missing id");
        }
        let url = self.map.url_for_key(id);
        let client = match Client::new(url) {
            Ok(c) => c,
            Err(e) => return error_body(502, "bad_gateway", &format!("shard url: {e}")),
        };
        match client.read(id) {
            Ok(resp) => match serde_json::to_vec(&resp) {
                Ok(b) => (200, "application/json", b),
                Err(e) => error_body(500, "internal", &format!("serialize: {e}")),
            },
            Err(e) => error_body(502, "bad_gateway", &format!("shard read: {e}")),
        }
    }

    /// Scatter-gather: collect records from every shard, merge to one JSONL
    /// stream. (Order is shard-then-record; the protocol does not promise a
    /// global order for `/iter`.)
    fn route_iter(&self) -> (u16, &'static str, Vec<u8>) {
        let mut out = Vec::new();
        for url in self.map.urls() {
            let client = match Client::new(url) {
                Ok(c) => c,
                Err(e) => return error_body(502, "bad_gateway", &format!("shard url: {e}")),
            };
            match client.iter() {
                Ok(records) => {
                    for rec in records {
                        match serde_json::to_vec(&rec) {
                            Ok(mut line) => {
                                out.append(&mut line);
                                out.push(b'\n');
                            }
                            Err(e) => {
                                return error_body(500, "internal", &format!("serialize: {e}"));
                            }
                        }
                    }
                }
                Err(e) => {
                    return error_body(502, "bad_gateway", &format!("shard {url} iter: {e}"));
                }
            }
        }
        (200, "application/jsonl", out)
    }
}

/// Strip a leading `/v1` segment, matching the shards' own aliasing.
fn canonicalize_v1(path: &str) -> &str {
    match path.strip_prefix("/v1") {
        Some(rest) if rest.starts_with('/') => rest,
        _ => path,
    }
}

fn error_body(status: u16, code: &str, detail: &str) -> (u16, &'static str, Vec<u8>) {
    let body = serde_json::json!({ "error": code, "detail": detail });
    (
        status,
        "application/json",
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        _ => "Status",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_for_key_is_stable_and_casing_insensitive() {
        let map = ShardMap::new(vec!["a".into(), "b".into(), "c".into()]);
        let id = "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
        let s1 = map.shard_for_key(id);
        let s2 = map.shard_for_key(&id.to_uppercase());
        let s3 = map.shard_for_key(&id.replace('-', ""));
        assert_eq!(s1, s2, "casing must not change the shard");
        assert_eq!(s1, s3, "hyphens must not change the shard");
        assert!(s1 < 3);
    }

    #[test]
    fn distribution_is_roughly_even() {
        let map = ShardMap::new(vec!["a".into(), "b".into(), "c".into(), "d".into()]);
        let mut counts = [0usize; 4];
        for i in 0..4000u64 {
            // Vary by index in the hashed key (no Uuid dep in lib tests).
            let key = format!("0190a1b2-c3d4-7e5f-8a9b-{i:012x}");
            counts[map.shard_for_key(&key)] += 1;
        }
        // Each shard should get roughly a quarter; allow generous slack.
        for c in counts {
            assert!(c > 700 && c < 1300, "uneven shard load: {counts:?}");
        }
    }
}
