//! nDB sharding coordinator.
//!
//! A stateless router that speaks the same `/v1` wire protocol to clients and
//! fans requests out across N single-writer shards (each a plain `ndb-server`).
//! Clients and the MCP server point at the router exactly as they would a
//! single server — the SDK is unchanged.
//!
//! Routing model (see `docs/superpowers/specs/2026-06-14-ndb-scale-sharding-design.md`):
//! - **Shard key:** `hash(entity_id) % N` (D1). UUIDv7 ids hash uniformly.
//! - **Point reads** (`/v1/read/:id`): hash-first to the owning shard, then
//!   scatter-on-miss (covers anchor-placed hyperedges).
//! - **Commits** (`/v1/commit`): split by routing key — entity → owning shard,
//!   hyperedge → anchor shard (D2), dictionary/metadata → broadcast.
//! - **Scans** (`/v1/iter`): scatter to every shard and merge.
//! - **Vector kNN** (`/v1/vector_search`): scatter the query, merge global top-k.
//!
//! Subsequent increments: cross-shard traversal (`neighbors`), online resharding.

use std::io::{BufRead, BufReader, Read, Write};
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
        // Read headers, capturing Content-Length so we can read a POST body.
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut req_body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut req_body)?;
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let raw_path = parts.next().unwrap_or("");
        let path = canonicalize_v1(raw_path.split('?').next().unwrap_or(""));

        let (status, ctype, body) = self.route(method, path, &req_body);
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
    fn route(&self, method: &str, path: &str, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
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
            ("POST", "/commit") => self.route_commit(body),
            ("POST", "/vector_search") => self.route_vector_search(body),
            _ => error_body(
                501,
                "not_implemented",
                "router increment: GET /v1/health, /v1/read/:id, /v1/iter, POST /v1/commit, POST /v1/vector_search are routed",
            ),
        }
    }

    /// Point read with hash-first + scatter-on-miss.
    ///
    /// An **entity** lives on `hash(entity_id)` → the owning shard answers on
    /// the fast path. A **hyperedge** lives on its anchor's shard (D2), which
    /// is `hash(anchor_entity_id)` ≠ `hash(hyperedge_id)` — so when the hashed
    /// shard doesn't have it, we scatter to the rest and return the first live
    /// hit. This keeps reads correct for both record kinds without the router
    /// knowing an id's kind up front.
    fn route_read(&self, id: &str) -> (u16, &'static str, Vec<u8>) {
        if id.is_empty() {
            return error_body(400, "bad_request", "missing id");
        }
        let owner = self.map.shard_for_key(id);
        // Fast path: the hash-owning shard.
        match self.read_from(owner, id) {
            Ok(Some(body)) => return (200, "application/json", body),
            Ok(None) => {}
            Err(e) => return error_body(502, "bad_gateway", &e),
        }
        // Miss: scatter to the other shards (covers anchor-placed hyperedges).
        for i in 0..self.map.len() {
            if i == owner {
                continue;
            }
            match self.read_from(i, id) {
                Ok(Some(body)) => return (200, "application/json", body),
                Ok(None) => {}
                Err(e) => return error_body(502, "bad_gateway", &e),
            }
        }
        // Not live on any shard — return the owning shard's (non-live) body.
        match self.read_from_raw(owner, id) {
            Ok(body) => (200, "application/json", body),
            Err(e) => error_body(502, "bad_gateway", &e),
        }
    }

    /// Read `id` from shard `i`; `Ok(Some(body))` if live, `Ok(None)` if the
    /// shard returned a non-live outcome, `Err` on transport failure.
    /// (`ReadResponse` is a serde-tagged enum on `outcome`; we test the tag in
    /// JSON rather than name the enum the router doesn't import.)
    fn read_from(&self, i: usize, id: &str) -> Result<Option<Vec<u8>>, String> {
        let resp = self
            .shard_client(i)?
            .read(id)
            .map_err(|e| format!("shard read: {e}"))?;
        let v = serde_json::to_value(&resp).map_err(|e| format!("serialize: {e}"))?;
        let is_live = v.get("outcome").and_then(serde_json::Value::as_str) == Some("live");
        let body = serde_json::to_vec(&v).map_err(|e| format!("serialize: {e}"))?;
        Ok(if is_live { Some(body) } else { None })
    }

    /// Read `id` from shard `i`, returning the serialized response regardless
    /// of outcome (used to surface a not-found body after a scatter miss).
    fn read_from_raw(&self, i: usize, id: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .shard_client(i)?
            .read(id)
            .map_err(|e| format!("shard read: {e}"))?;
        serde_json::to_vec(&resp).map_err(|e| format!("serialize: {e}"))
    }

    fn shard_client(&self, i: usize) -> Result<Client, String> {
        Client::new(&self.map.urls()[i]).map_err(|e| format!("shard url: {e}"))
    }

    /// Commit routing (D1 + D2). Splits the batch by routing key and sends a
    /// per-shard sub-commit:
    /// - **entity** → `hash(entity_id)`
    /// - **hyperedge** → `hash(anchor)` where anchor = the first role-filler's
    ///   entity id (D2 anchor placement); falls back to `hyperedge_id` if it
    ///   has no entity role-fillers.
    /// - **tombstone** → `hash(target_id)`
    /// - **dictionary / policy / metadata** (any other `kind`) → **broadcast**
    ///   to every shard. These carry caller-assigned ids, so broadcasting keeps
    ///   the per-shard dictionaries consistent.
    ///
    /// No distributed transaction (D4): each shard's sub-commit is atomic on
    /// that shard. If a sub-commit fails after others succeeded, that is a
    /// partial commit — reported as `502 partial_commit`, not rolled back.
    fn route_commit(&self, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
        let parsed: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return error_body(400, "bad_request", &format!("invalid JSON: {e}")),
        };
        let Some(records) = parsed.get("records").and_then(|r| r.as_array()) else {
            return error_body(400, "bad_request", "missing records array");
        };

        let n = self.map.len();
        let mut per_shard: Vec<Vec<serde_json::Value>> = vec![Vec::new(); n];
        let mut globals: Vec<serde_json::Value> = Vec::new();
        for rec in records {
            match routing_key(rec) {
                Some(key) => per_shard[self.map.shard_for_key(&key)].push(rec.clone()),
                None => globals.push(rec.clone()), // dictionary / metadata → all shards
            }
        }

        let mut max_tx: u64 = 0;
        let mut failures: Vec<String> = Vec::new();
        let mut committed_shards = 0usize;
        for (i, keyed) in per_shard.into_iter().enumerate() {
            if keyed.is_empty() && globals.is_empty() {
                continue;
            }
            let mut recs = globals.clone();
            recs.extend(keyed);
            let sub = serde_json::json!({ "records": recs });
            let payload = serde_json::to_vec(&sub).unwrap_or_default();
            match post_to_shard(&self.map.urls()[i], "/v1/commit", &payload) {
                Ok((status, resp)) if (200..300).contains(&status) => {
                    committed_shards += 1;
                    if let Some(tx) = serde_json::from_slice::<serde_json::Value>(&resp)
                        .ok()
                        .and_then(|v| v.get("tx_id").and_then(serde_json::Value::as_u64))
                    {
                        max_tx = max_tx.max(tx);
                    }
                }
                Ok((status, resp)) => failures.push(format!(
                    "shard {i} status {status}: {}",
                    String::from_utf8_lossy(&resp)
                )),
                Err(e) => failures.push(format!("shard {i}: {e}")),
            }
        }

        if !failures.is_empty() {
            return error_body(
                502,
                "partial_commit",
                &format!(
                    "{committed_shards} shard(s) committed, {} failed: {}",
                    failures.len(),
                    failures.join("; ")
                ),
            );
        }
        let body = serde_json::json!({ "tx_id": max_tx });
        (
            200,
            "application/json",
            serde_json::to_vec(&body).unwrap_or_default(),
        )
    }

    /// Vector kNN across shards: scatter the SAME query to every shard, then
    /// **merge top-k**. Each shard returns its own ascending-by-distance hits;
    /// the global top-k is the k smallest-distance hits of the union (distance
    /// is smaller-is-closer for every metric, so one ascending sort suffices —
    /// no metric branching). A shard failure fails the whole query (an
    /// incomplete kNN would silently return a wrong ranking).
    fn route_vector_search(&self, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
        let req: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return error_body(400, "bad_request", &format!("invalid JSON: {e}")),
        };
        let k = req
            .get("k")
            .and_then(serde_json::Value::as_u64)
            .map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));

        let mut hits: Vec<serde_json::Value> = Vec::new();
        for url in self.map.urls() {
            match post_to_shard(url, "/v1/vector_search", body) {
                Ok((status, resp)) if (200..300).contains(&status) => {
                    let parsed: serde_json::Value = serde_json::from_slice(&resp)
                        .unwrap_or_else(|_| serde_json::json!({ "hits": [] }));
                    if let Some(arr) = parsed.get("hits").and_then(|h| h.as_array()) {
                        hits.extend(arr.iter().cloned());
                    }
                }
                Ok((status, resp)) => {
                    return error_body(
                        502,
                        "bad_gateway",
                        &format!(
                            "shard {url} vector_search status {status}: {}",
                            String::from_utf8_lossy(&resp)
                        ),
                    );
                }
                Err(e) => return error_body(502, "bad_gateway", &format!("shard {url}: {e}")),
            }
        }
        // Ascending by distance (missing distance sorts last).
        hits.sort_by(|a, b| {
            let da = a
                .get("distance")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(f64::INFINITY);
            let db = b
                .get("distance")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(f64::INFINITY);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(k);
        let out = serde_json::json!({ "hits": hits });
        (
            200,
            "application/json",
            serde_json::to_vec(&out).unwrap_or_default(),
        )
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

/// The routing key for a commit record: which entity/edge id decides its
/// shard. `None` means "no key" → a global record (dictionary / metadata) that
/// must be broadcast to every shard.
fn routing_key(rec: &serde_json::Value) -> Option<String> {
    let s = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
    match rec.get("kind").and_then(serde_json::Value::as_str) {
        Some("entity") => s(rec, "entity_id"),
        Some("hyper_edge") => {
            // Anchor = first role-filler's entity id (D2); fall back to the
            // hyperedge's own id if it has no entity role-fillers.
            rec.get("roles")
                .and_then(|r| r.as_array())
                .and_then(|a| a.first())
                .and_then(|r0| r0.get("entity_id"))
                .and_then(|x| x.as_str())
                .map(str::to_owned)
                .or_else(|| s(rec, "hyperedge_id"))
        }
        Some("tombstone") => s(rec, "target_id"),
        // Dictionary (type_name/role_name/property_key) + policy/metadata: no
        // routing key → broadcast (they carry caller-assigned ids).
        _ => None,
    }
}

/// Minimal HTTP POST to a shard (`url` like `http://host:port`). Returns the
/// shard's `(status, body)`. One request per connection (`Connection: close`).
fn post_to_shard(url: &str, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let host_port = url
        .strip_prefix("http://")
        .unwrap_or(url)
        .trim_end_matches('/');
    let mut stream =
        TcpStream::connect(host_port).map_err(|e| format!("connect {host_port}: {e}"))?;
    let header = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.write_all(body).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "no header terminator from shard".to_string())?;
    let status = std::str::from_utf8(&raw[..sep])
        .ok()
        .and_then(|h| h.lines().next())
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| "unparseable shard status".to_string())?;
    Ok((status, raw[sep + 4..].to_vec()))
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
