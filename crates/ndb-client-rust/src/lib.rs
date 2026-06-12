//! nDB Rust client — HTTP wire-protocol library.
//!
//! Provides a typed [`Client`] that mirrors the [`ndb-server`] route set.
//! Same surface as the Python client in `clients/python/`, but typed
//! against the engine's `wire::*` shapes so misspelled fields fail at
//! compile time.
//!
//! # v1 design baked in here
//!
//! - **`std::net::TcpStream` only.** No tokio, no async — matches the
//!   single-writer ergonomics of the server. Calls block on the response.
//!   Drop-in for any Rust application that wants nDB via wire protocol
//!   without an async runtime.
//!
//! - **Wire types come from `ndb-engine`.** `CommitRequest`,
//!   `JsonRecord`, `LookupResponse`, `VectorHit`, etc. are the
//!   server's serde types; we reuse them directly. The compiler enforces
//!   the contract.
//!
//! - **Iter is `Iterator<Item = JsonRecord>`.** `iter()` returns an
//!   eager-loaded `Vec<JsonRecord>` in v1 (collects from the JSONL
//!   stream up-front). True streaming (`Read` over the open socket)
//!   lands when an application surfaces a measurable hot path.
//!
//! - **Errors classify into 3 buckets.** [`ClientError::Io`] for
//!   network-layer failures; [`ClientError::Http`] for the server's
//!   structured 4xx/5xx body; [`ClientError::Parse`] for unexpected
//!   shapes.
//!
//! # Example
//!
//! ```no_run
//! use ndb_client::Client;
//!
//! let ndb = Client::new("http://127.0.0.1:8742").unwrap();
//! let status = ndb.health().unwrap();
//! assert_eq!(status.status, "ok");
//! ```

#![warn(missing_docs)]
#![allow(clippy::doc_markdown)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use ndb_engine::{
    CommitRequest, CommitResponse, ErrorResponse, JsonRecord, JsonValue, LookupRequest,
    LookupResponse, PropertyLookupRequest, PropertyLookupResponse, PropertyRangeRequest,
    PropertyRangeResponse, QueryRequest, QueryResponse, ReadResponse, TraverseHop, TraverseRequest,
    TraverseResponse, VectorHit, VectorMetric, VectorSearchRequest, VectorSearchResponse,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default server URL (matches `ndb-server`'s default bind).
pub const DEFAULT_URL: &str = "http://127.0.0.1:8742";

/// Errors raised by the client.
#[derive(Debug, Error)]
pub enum ClientError {
    /// Network-layer failure (DNS, refused, timeout).
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Server returned a non-2xx response.
    #[error("HTTP {status} {error}: {detail}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Short machine-readable error tag from `ErrorResponse`.
        error: String,
        /// Human-readable detail.
        detail: String,
    },

    /// Couldn't parse the response body (unexpected shape, malformed JSON).
    #[error("response parse error: {0}")]
    Parse(String),

    /// Malformed URL.
    #[error("invalid URL: {0}")]
    BadUrl(String),
}

/// `/health` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Liveness status; `"ok"` when the server is up.
    pub status: String,
}

/// `/flush` response — memtable + SSTable counts after the flush.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlushResponse {
    /// Records left in the memtable (typically 0 right after flush).
    pub memtable_records: u64,
    /// Memtable byte footprint.
    pub memtable_bytes: u64,
    /// Total open SSTables after the flush.
    pub sstable_count: usize,
}

/// `/compact` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactResponse {
    /// Records consumed from the input SSTables.
    pub records_in: u64,
    /// Records emitted into the new SSTable.
    pub records_out: u64,
    /// Input SSTable count.
    pub sstables_in: usize,
    /// `file_seq` of the new compacted SSTable.
    pub new_sstable_seq: Option<u64>,
}

/// nDB HTTP client.
#[derive(Debug, Clone)]
pub struct Client {
    host_port: String,
    /// Raw bearer token (without the `Bearer ` prefix). Empty = no auth.
    token: String,
    read_timeout: Duration,
    write_timeout: Duration,
}

impl Client {
    /// Build a client pointed at `url`. Accepts `http://host:port` or
    /// bare `host:port`; reads `NDB_TOKEN` from the env for auth.
    pub fn new(url: &str) -> Result<Self, ClientError> {
        let host_port = parse_host_port(url).ok_or_else(|| ClientError::BadUrl(url.to_owned()))?;
        let token = std::env::var("NDB_TOKEN").unwrap_or_default();
        Ok(Self {
            host_port,
            token,
            read_timeout: Duration::from_mins(1),
            write_timeout: Duration::from_secs(30),
        })
    }

    /// Override the read timeout (default: 60s).
    #[must_use]
    pub fn with_read_timeout(mut self, t: Duration) -> Self {
        self.read_timeout = t;
        self
    }

    /// Override the write timeout (default: 30s).
    #[must_use]
    pub fn with_write_timeout(mut self, t: Duration) -> Self {
        self.write_timeout = t;
        self
    }

    /// Override the bearer token. Empty `t` removes any prior token
    /// (including one inherited from `NDB_TOKEN`).
    #[must_use]
    pub fn with_token(mut self, t: impl Into<String>) -> Self {
        self.token = t.into();
        self
    }

    /// Resolved `host:port` the client will connect to.
    #[must_use]
    pub fn host_port(&self) -> &str {
        &self.host_port
    }

    // -----------------------------------------------------------------
    // Routes.
    // -----------------------------------------------------------------

    /// `GET /health` — liveness probe.
    pub fn health(&self) -> Result<HealthResponse, ClientError> {
        let (status, body) = self.get("/health")?;
        parse_2xx(status, &body)
    }

    /// `POST /commit` — commit a batch of records.
    pub fn commit(&self, req: &CommitRequest) -> Result<CommitResponse, ClientError> {
        let body = serde_json::to_vec(req).map_err(|e| ClientError::Parse(e.to_string()))?;
        let (status, resp) = self.post("/commit", &body)?;
        parse_2xx(status, &resp)
    }

    /// `GET /read/:uuid` — point lookup at the latest snapshot.
    pub fn read(&self, uuid: &str) -> Result<ReadResponse, ClientError> {
        let (status, body) = self.get(&format!("/read/{uuid}"))?;
        parse_2xx(status, &body)
    }

    /// `GET /iter` — collect every visible record at the latest snapshot.
    ///
    /// Loads the entire stream into memory before returning. For large
    /// databases prefer paginating via `/lookup` + `/property_*` queries.
    #[allow(clippy::iter_not_returning_iterator)] // method mirrors HTTP route
    pub fn iter(&self) -> Result<Vec<JsonRecord>, ClientError> {
        let (status, body) = self.get("/iter")?;
        if !(200..300).contains(&status) {
            return Err(http_error(status, &body));
        }
        let text = std::str::from_utf8(&body)
            .map_err(|e| ClientError::Parse(format!("iter body not utf8: {e}")))?;
        let mut out = Vec::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let rec: JsonRecord = serde_json::from_str(line)
                .map_err(|e| ClientError::Parse(format!("iter line: {e}")))?;
            out.push(rec);
        }
        Ok(out)
    }

    /// `POST /flush` — drain the memtable into a new SSTable.
    pub fn flush(&self) -> Result<FlushResponse, ClientError> {
        let (status, body) = self.post("/flush", b"")?;
        parse_2xx(status, &body)
    }

    /// `POST /compact` — full compaction across open SSTables.
    pub fn compact(&self) -> Result<CompactResponse, ClientError> {
        let (status, body) = self.post("/compact", b"")?;
        parse_2xx(status, &body)
    }

    /// `POST /lookup` — find entity by external lookup-key. Returns the
    /// uuid as a string, or `None` if no entity matches.
    pub fn lookup_by_key(
        &self,
        property_id: u32,
        value: JsonValue,
    ) -> Result<Option<String>, ClientError> {
        let req = LookupRequest { property_id, value };
        let body = serde_json::to_vec(&req).map_err(|e| ClientError::Parse(e.to_string()))?;
        let (status, resp) = self.post("/lookup", &body)?;
        let parsed: LookupResponse = parse_2xx(status, &resp)?;
        Ok(parsed.entity_id)
    }

    /// `POST /vector_search` — k-NN over a vector-indexed property.
    pub fn vector_search(
        &self,
        property_id: u32,
        query: &[f32],
        k: usize,
        metric: VectorMetric,
    ) -> Result<Vec<VectorHit>, ClientError> {
        let req = VectorSearchRequest {
            property_id,
            query: query.to_vec(),
            k,
            metric,
        };
        let body = serde_json::to_vec(&req).map_err(|e| ClientError::Parse(e.to_string()))?;
        let (status, resp) = self.post("/vector_search", &body)?;
        let parsed: VectorSearchResponse = parse_2xx(status, &resp)?;
        Ok(parsed.hits)
    }

    /// `POST /property_lookup` — exact match on `(type, property, value)`.
    pub fn property_lookup(
        &self,
        type_id: u32,
        property_id: u32,
        value: JsonValue,
    ) -> Result<Vec<String>, ClientError> {
        let req = PropertyLookupRequest {
            type_id,
            property_id,
            value,
        };
        let body = serde_json::to_vec(&req).map_err(|e| ClientError::Parse(e.to_string()))?;
        let (status, resp) = self.post("/property_lookup", &body)?;
        let parsed: PropertyLookupResponse = parse_2xx(status, &resp)?;
        Ok(parsed.entity_ids)
    }

    /// `POST /traverse` — server-side multi-hop BFS over hyperedges.
    ///
    /// Walks from `start_uuid` through the configured sequence of
    /// `hyperedge_type_id` filters and returns every entity reachable at
    /// the final hop. Single round-trip regardless of fanout — the
    /// server runs the traversal in-process against the adjacency index.
    pub fn traverse(
        &self,
        start_uuid: &str,
        hops: Vec<TraverseHop>,
    ) -> Result<Vec<String>, ClientError> {
        let req = TraverseRequest {
            start: start_uuid.to_owned(),
            hops,
        };
        let body = serde_json::to_vec(&req).map_err(|e| ClientError::Parse(e.to_string()))?;
        let (status, resp) = self.post("/traverse", &body)?;
        let parsed: TraverseResponse = parse_2xx(status, &resp)?;
        Ok(parsed.entity_ids)
    }

    /// `POST /query` — execute a structured wire-AST query.
    ///
    /// The request is a [`QueryRequest`] (id-based; the resolver step
    /// converting names → ids lives in `ndb-query` and is the caller's
    /// responsibility for v1). The response includes the projected
    /// columns, one row per result tuple, and a `truncated` flag if
    /// `limit` capped the result.
    pub fn query(&self, req: &QueryRequest) -> Result<QueryResponse, ClientError> {
        let body = serde_json::to_vec(req).map_err(|e| ClientError::Parse(e.to_string()))?;
        let (status, resp) = self.post("/query", &body)?;
        parse_2xx(status, &resp)
    }

    /// `POST /query/text` — execute query SOURCE TEXT against the server.
    ///
    /// The server lexes + parses + resolves names → ids using its own
    /// dictionary snapshot, then runs the resulting wire-AST. Use this
    /// over [`Self::query`] when the caller has a query string in hand
    /// (CLI / REPL / SPA filter) and doesn't want to ship the parser to
    /// the client.
    pub fn query_text(&self, text: &str) -> Result<QueryResponse, ClientError> {
        let (status, resp) = self.post("/query/text", text.as_bytes())?;
        parse_2xx(status, &resp)
    }

    /// `POST /property_range` — range query on `(type, property)`.
    /// Both bounds inclusive; `None` = unbounded.
    pub fn property_range(
        &self,
        type_id: u32,
        property_id: u32,
        low: Option<JsonValue>,
        high: Option<JsonValue>,
    ) -> Result<Vec<String>, ClientError> {
        let req = PropertyRangeRequest {
            type_id,
            property_id,
            low,
            high,
        };
        let body = serde_json::to_vec(&req).map_err(|e| ClientError::Parse(e.to_string()))?;
        let (status, resp) = self.post("/property_range", &body)?;
        let parsed: PropertyRangeResponse = parse_2xx(status, &resp)?;
        Ok(parsed.entity_ids)
    }

    // -----------------------------------------------------------------
    // HTTP plumbing.
    // -----------------------------------------------------------------

    fn connect(&self) -> std::io::Result<TcpStream> {
        let stream = TcpStream::connect(&self.host_port)?;
        stream.set_read_timeout(Some(self.read_timeout))?;
        stream.set_write_timeout(Some(self.write_timeout))?;
        Ok(stream)
    }

    fn auth_header(&self) -> String {
        if self.token.is_empty() {
            String::new()
        } else {
            format!("Authorization: Bearer {}\r\n", self.token)
        }
    }

    fn get(&self, path: &str) -> Result<(u16, Vec<u8>), ClientError> {
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\n{}Connection: close\r\n\r\n",
            self.host_port,
            self.auth_header()
        );
        self.issue(req.as_bytes())
    }

    fn post(&self, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>), ClientError> {
        let mut req = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.host_port,
            self.auth_header(),
            body.len(),
        )
        .into_bytes();
        req.extend_from_slice(body);
        self.issue(&req)
    }

    fn issue(&self, request: &[u8]) -> Result<(u16, Vec<u8>), ClientError> {
        let mut s = self.connect()?;
        s.write_all(request)?;
        s.flush()?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf)?;
        let header_end = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| ClientError::Parse("no HTTP header terminator".to_owned()))?;
        let head = std::str::from_utf8(&buf[..header_end])
            .map_err(|_| ClientError::Parse("non-utf8 HTTP head".to_owned()))?;
        let status: u16 = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| ClientError::Parse("no status code".to_owned()))?;
        Ok((status, buf[header_end + 4..].to_vec()))
    }
}

fn parse_2xx<T: serde::de::DeserializeOwned>(status: u16, body: &[u8]) -> Result<T, ClientError> {
    if !(200..300).contains(&status) {
        return Err(http_error(status, body));
    }
    serde_json::from_slice(body).map_err(|e| ClientError::Parse(e.to_string()))
}

fn http_error(status: u16, body: &[u8]) -> ClientError {
    let (error, detail) = serde_json::from_slice::<ErrorResponse>(body).map_or_else(
        |_| {
            (
                "http_error".to_owned(),
                String::from_utf8_lossy(body).into_owned(),
            )
        },
        |e| (e.error, e.detail),
    );
    ClientError::Http {
        status,
        error,
        detail,
    }
}

fn parse_host_port(url: &str) -> Option<String> {
    // Accept "http://host:port", "host:port", with optional trailing /
    let stripped = url.strip_prefix("http://").unwrap_or(url);
    let stripped = stripped.strip_suffix('/').unwrap_or(stripped);
    if stripped.contains('/') {
        return None;
    }
    Some(stripped.to_owned())
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn parses_url_with_scheme() {
        assert_eq!(
            parse_host_port("http://127.0.0.1:8742").unwrap(),
            "127.0.0.1:8742"
        );
    }

    #[test]
    fn parses_bare_host_port() {
        assert_eq!(parse_host_port("localhost:9000").unwrap(), "localhost:9000");
    }

    #[test]
    fn strips_trailing_slash() {
        assert_eq!(parse_host_port("http://x:1/").unwrap(), "x:1");
    }

    #[test]
    fn rejects_paths_in_url() {
        assert!(parse_host_port("http://x:1/foo").is_none());
    }

    #[test]
    fn http_error_carries_structured_body() {
        let err = http_error(401, br#"{"error":"unauthorized","detail":"missing token"}"#);
        match err {
            ClientError::Http {
                status,
                error,
                detail,
            } => {
                assert_eq!(status, 401);
                assert_eq!(error, "unauthorized");
                assert_eq!(detail, "missing token");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn http_error_falls_back_to_raw_body() {
        let err = http_error(500, b"<html>internal server</html>");
        match err {
            ClientError::Http { error, detail, .. } => {
                assert_eq!(error, "http_error");
                assert!(detail.contains("<html>"));
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }
}
