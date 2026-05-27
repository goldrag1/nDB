//! nDB HTTP server — wire-protocol bridge to [`ndb_engine::Engine`].
#![warn(missing_docs)]
#![allow(clippy::doc_markdown)] // "Engine", "SSTable", "WAL", "JSONL" used liberally.
//!
//! v1 surface (intentionally narrow, hand-rolled HTTP/1.1):
//!
//! - `GET  /health` — liveness; responds `200 {"status":"ok"}`.
//! - `POST /commit` — body `CommitRequest`; commits records; responds
//!   `CommitResponse { tx_id }`.
//! - `GET  /read/:uuid` — looks up a UUID at the latest snapshot;
//!   responds `ReadResponse { outcome: missing|deleted|live, ... }`.
//! - `GET  /iter` — streams every visible record at the latest snapshot
//!   as JSONL (one `JsonRecord` per line, `Content-Type: application/jsonl`).
//!
//! v1 design decisions, locked here:
//!
//! - **Hand-rolled `std::net` HTTP/1.1.** Single-threaded, no tokio, no
//!   async, no axum. Matches the engine's single-writer model exactly
//!   (§14.3) and keeps the dependency footprint tiny. We can swap in
//!   axum/tokio in v2 if real concurrency demand emerges.
//! - **Engine wrapped in a `Mutex`.** Single-writer means the engine
//!   handle is `&mut self` for writes; the server's request loop takes
//!   the mutex per request. Long requests (e.g. /iter on a big database)
//!   will block other writers — acceptable for v1, fixable in v2 with
//!   a request queue.
//! - **No authentication, no TLS.** Bind to `127.0.0.1` by default.
//!   Security baseline (§13) is its own commit.
//! - **No request body size limit.** The `Content-Length` header is
//!   honored; chunked transfer is not supported. v1 expects polite
//!   clients.
//!
//! Run via:
//!
//! ```text
//! cargo run -p ndb-server -- --path /tmp/mydb --bind 127.0.0.1:8742
//! ```

use std::collections::{BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ndb_engine::{
    CommitRequest, CommitResponse, Engine, EngineError, ErrorResponse, JsonRecord, JsonValue,
    LookupRequest, LookupResponse, PropertyLookupRequest, PropertyLookupResponse,
    PropertyRangeRequest, PropertyRangeResponse, QueryError, QueryRequest, ReadResponse, Record,
    Resolved, SubscribeRequest, TraverseRequest, TraverseResponse, TxId, VectorHit, VectorMetric,
    VectorSearchRequest, VectorSearchResponse, WireError, WriteTxn, execute_query,
};
use ndb_engine::id::{EntityId, PropertyId, TypeId};
use ndb_engine::index::Distance;
use ndb_engine::value::Value;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Audit log filename, written under the database directory.
pub const AUDIT_LOG_FILENAME: &str = ".audit.jsonl";

/// Hard cap on `k` for `/vector_search` to prevent a single client from
/// streaming an unbounded result set. 1000 is enough to support every
/// reasonable RAG workload; callers that genuinely need more should
/// paginate.
pub const MAX_VECTOR_K: usize = 1000;

/// Principals config filename, optionally placed under the database directory.
pub const PRINCIPALS_FILENAME: &str = ".principals.json";

// ---------------------------------------------------------------------------
// ReBAC capability model
// ---------------------------------------------------------------------------

/// Coarse-grained capability tokens used by the server to gate routes (and
/// by the MCP server to gate tools).
///
/// v1 captures direct capabilities only — no inference, no transitive reach
/// (§13.2). The mapping principal → capability set is shipped as a small
/// in-memory table loaded from `<db>/.principals.json` on `with_principals_*`.
/// v2 will migrate this to capability hyperedges stored in the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// `GET /health` — always allowed, listed for completeness.
    Health,
    /// `GET /read/:uuid`.
    Read,
    /// `GET /iter`.
    Iter,
    /// `POST /commit`.
    Commit,
    /// `POST /flush`.
    Flush,
    /// `POST /compact`.
    Compact,
    /// Wildcard — implies every other capability. Use sparingly.
    Admin,
}

/// One row in the principals table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    /// Stable display name (used in audit logs and error messages).
    pub name: String,
    /// Direct capability grants.
    pub capabilities: BTreeSet<Capability>,
}

impl Principal {
    /// True iff this principal can perform `cap` (either directly or via
    /// the `Admin` wildcard).
    #[must_use]
    pub fn allows(&self, cap: Capability) -> bool {
        self.capabilities.contains(&Capability::Admin) || self.capabilities.contains(&cap)
    }
}

/// Principal registry — token → principal mapping.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Principals {
    /// Map from bearer token to principal record. Tokens are opaque; their
    /// only structural requirement is being non-empty.
    pub principals: HashMap<String, Principal>,
}

impl Principals {
    /// Load a principals file from disk. Returns `Ok(None)` if the file
    /// doesn't exist (caller decides whether that's fatal).
    pub fn load(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let p: Self = serde_json::from_slice(&bytes).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })?;
                Ok(Some(p))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Look up a principal by raw bearer token (constant-time over the set
    /// — but indexed: each hit takes O(token-len) to compare, the HashMap
    /// scan is unavoidable for the constant-time-equality guarantee).
    #[must_use]
    pub fn resolve(&self, token: &str) -> Option<&Principal> {
        // Walk every entry so a token that's a prefix of another doesn't
        // short-circuit. Constant-time-compare each candidate.
        let mut found: Option<&Principal> = None;
        let tok = token.as_bytes();
        for (k, p) in &self.principals {
            if constant_time_eq(k.as_bytes(), tok) {
                found = Some(p);
            }
        }
        found
    }
}

/// Errors raised by the server.
#[derive(Debug, Error)]
pub enum ServerError {
    /// I/O failure during accept / read / write.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Engine-level failure (commit, read, validation).
    #[error(transparent)]
    Engine(#[from] EngineError),

    /// Wire-format parse or convert failure.
    #[error(transparent)]
    Wire(#[from] WireError),

    /// Failed to parse incoming HTTP request.
    #[error("malformed HTTP request: {0}")]
    BadRequest(&'static str),
}

/// Server handle wrapping a shared engine.
pub struct Server {
    engine: Arc<Mutex<Engine>>,
    /// Optional bearer token. If `Some` AND `principals` is `None`, every
    /// request must carry `Authorization: Bearer <token>` else 401. v1
    /// keeps this single-token path for backward compatibility with the
    /// initial wire-protocol release.
    auth_token: Option<String>,
    /// Optional principals registry. When present, overrides the single
    /// `auth_token` path: each request must carry a recognised bearer
    /// token AND the resolved principal must hold the route's capability.
    principals: Option<Principals>,
    /// Append-only `.audit.jsonl` under the database directory. Every
    /// dispatched request gets one line. None when auditing is disabled.
    audit: Option<Arc<Mutex<AuditLog>>>,
    /// Optional pre-built rustls `ServerConfig`. When present, the server
    /// can be bound via [`run_tls`](Self::run_tls) / [`bind_tls`](Self::bind_tls)
    /// to terminate TLS itself instead of relying on a reverse proxy
    /// (§13.3). When absent, only the plain-TCP paths are available.
    tls_config: Option<Arc<rustls::ServerConfig>>,
    /// Condvar-based commit notification (v2.0+). The mutex holds the
    /// latest committed tx_id seen by this server; `notify_all` fires on
    /// every successful commit. `/subscribe` blocks on this condvar
    /// instead of polling every 50ms — sub-millisecond latency.
    commit_notify: Arc<(Mutex<u64>, std::sync::Condvar)>,
}

/// Append-only audit log. One JSON line per request. Synchronous flush
/// after every write so a crash loses at most the in-flight line.
#[derive(Debug)]
pub struct AuditLog {
    file: std::fs::File,
    path: PathBuf,
}

impl AuditLog {
    /// Open or create `<db>/.audit.jsonl` for append.
    pub fn open(db_dir: &Path) -> std::io::Result<Self> {
        let path = db_dir.join(AUDIT_LOG_FILENAME);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self { file, path })
    }

    /// Path to the audit log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record, then flush.
    pub fn append(&mut self, entry: &AuditEntry<'_>) -> std::io::Result<()> {
        let line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }
}

/// One row in the audit log. Field shape is intentionally stable —
/// downstream SIEM pipelines key off these names.
#[derive(Debug, Serialize)]
pub struct AuditEntry<'a> {
    /// Unix epoch microseconds.
    pub ts_us: u128,
    /// Principal name (from token mapping) or `"anonymous"` when auth is off.
    pub principal: &'a str,
    /// HTTP method (uppercase).
    pub method: &'a str,
    /// Request path (no query).
    pub path: &'a str,
    /// Response status code.
    pub status: u16,
    /// Transaction id, present only for successful `/commit` calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_id: Option<u64>,
    /// Optional failure reason for non-2xx responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<&'a str>,
}

fn now_micros() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros())
}

impl Server {
    /// Open an existing database (or create one if missing) and prepare
    /// the server for `run` / handle_connection. Authentication is off
    /// by default; call [`with_auth_token`](Self::with_auth_token).
    ///
    /// At-rest encryption is sourced from `NDB_ENC_KEY` — if set, the
    /// engine encrypts new files (on create) or refuses to open unless
    /// the marker fingerprint matches.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, ServerError> {
        let path = path.as_ref();
        let engine = if path.exists() && path.join("CURRENT").exists() {
            Engine::open_from_env(path)?
        } else {
            Engine::create_from_env(path)?
        };
        let initial_tx = engine.manifest().last_tx_id;
        Ok(Self {
            engine: Arc::new(Mutex::new(engine)),
            auth_token: None,
            principals: None,
            audit: None,
            tls_config: None,
            commit_notify: Arc::new((Mutex::new(initial_tx), std::sync::Condvar::new())),
        })
    }

    /// Wrap an already-opened engine. Useful for tests.
    #[must_use]
    pub fn from_engine(engine: Engine) -> Self {
        let initial_tx = engine.manifest().last_tx_id;
        Self {
            engine: Arc::new(Mutex::new(engine)),
            auth_token: None,
            principals: None,
            audit: None,
            tls_config: None,
            commit_notify: Arc::new((Mutex::new(initial_tx), std::sync::Condvar::new())),
        }
    }

    /// Require an `Authorization: Bearer <token>` header on every
    /// request. Empty `token` removes the requirement.
    #[must_use]
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        let t: String = token.into();
        self.auth_token = if t.is_empty() { None } else { Some(t) };
        self
    }

    /// Install a principals registry. Overrides any prior bearer-token
    /// configuration. Once installed, every route except `/health`
    /// requires a recognised bearer token AND the matching principal must
    /// hold the route's capability.
    #[must_use]
    pub fn with_principals(mut self, p: Principals) -> Self {
        self.principals = Some(p);
        self
    }

    /// Convenience: look for `<db>/.principals.json` and install it if
    /// present. Returns `Ok(self, true)` if a file was loaded; `Ok(self,
    /// false)` if the file was absent (no change to the server); error on
    /// any other I/O or parse failure.
    pub fn with_principals_from_db(self) -> Result<(Self, bool), ServerError> {
        let dir = {
            let eng = self.engine.lock().expect("engine mutex poisoned");
            eng.path().to_path_buf()
        };
        let path = dir.join(PRINCIPALS_FILENAME);
        match Principals::load(&path)? {
            Some(p) => Ok((self.with_principals(p), true)),
            None => Ok((self, false)),
        }
    }

    /// Install a pre-built rustls `ServerConfig`. Once present, the
    /// server gains TLS-bind / TLS-run methods. Plain-TCP routes still
    /// work in parallel.
    #[must_use]
    pub fn with_tls(mut self, cfg: Arc<rustls::ServerConfig>) -> Self {
        self.tls_config = Some(cfg);
        self
    }

    /// Convenience: load a PEM-encoded certificate chain and PKCS#8
    /// private key from disk, build a rustls `ServerConfig` with safe
    /// defaults (TLS 1.2 + 1.3, ring-backed crypto), and install it.
    pub fn with_tls_pem(self, cert_path: &Path, key_path: &Path) -> Result<Self, ServerError> {
        let cfg = build_rustls_config(cert_path, key_path)?;
        Ok(self.with_tls(Arc::new(cfg)))
    }

    /// Bind a TLS listener on `addr`. Returns an [`BoundTlsServer`].
    pub fn bind_tls<A: ToSocketAddrs>(&self, addr: A) -> Result<BoundTlsServer<'_>, ServerError> {
        let cfg = self
            .tls_config
            .clone()
            .ok_or(ServerError::BadRequest("TLS not configured"))?;
        let listener = TcpListener::bind(addr)?;
        Ok(BoundTlsServer {
            server: self,
            listener,
            cfg,
        })
    }

    /// Block forever accepting TLS connections on `addr`.
    pub fn run_tls<A: ToSocketAddrs>(&self, addr: A) -> Result<(), ServerError> {
        let bound = self.bind_tls(addr)?;
        bound.serve()
    }

    /// Enable audit logging. Every dispatched request appends one line
    /// to `<db>/.audit.jsonl`. Auditing is best-effort: failures to write
    /// to the audit file are logged to stderr but do NOT fail the request
    /// (so a full disk on the audit volume does not take the server down).
    pub fn with_audit_log(mut self) -> Result<Self, ServerError> {
        let dir = {
            let eng = self.engine.lock().expect("engine mutex poisoned");
            eng.path().to_path_buf()
        };
        let log = AuditLog::open(&dir)?;
        self.audit = Some(Arc::new(Mutex::new(log)));
        Ok(self)
    }

    /// Path of the open audit log, if any.
    #[must_use]
    pub fn audit_log_path(&self) -> Option<PathBuf> {
        self.audit
            .as_ref()
            .map(|a| a.lock().expect("audit mutex poisoned").path().to_path_buf())
    }

    fn record_audit(
        &self,
        principal: &str,
        method: &str,
        path: &str,
        status: u16,
        tx_id: Option<u64>,
        failure: Option<&str>,
    ) {
        if let Some(log) = &self.audit {
            let entry = AuditEntry {
                ts_us: now_micros(),
                principal,
                method,
                path,
                status,
                tx_id,
                failure,
            };
            if let Err(e) = log.lock().expect("audit mutex poisoned").append(&entry) {
                eprintln!("audit log write failed: {e}");
            }
        }
    }

    /// Block forever accepting connections on `addr`.
    pub fn run<A: ToSocketAddrs>(&self, addr: A) -> Result<(), ServerError> {
        let listener = TcpListener::bind(addr)?;
        eprintln!("ndb-server listening on {}", listener.local_addr()?);
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    if let Err(e) = self.handle_connection(s) {
                        eprintln!("connection error: {e}");
                    }
                }
                Err(e) => eprintln!("accept error: {e}"),
            }
        }
        Ok(())
    }

    /// Bind and return the bound address. Used by tests so they can
    /// pick an ephemeral port (`127.0.0.1:0`) and learn what it became.
    pub fn bind<A: ToSocketAddrs>(&self, addr: A) -> Result<BoundServer<'_>, ServerError> {
        let listener = TcpListener::bind(addr)?;
        Ok(BoundServer {
            server: self,
            listener,
        })
    }

    /// Handle one plain-TCP connection. Convenience wrapper for the
    /// generic [`handle_io`](Self::handle_io).
    pub fn handle_connection(&self, stream: TcpStream) -> Result<(), ServerError> {
        // BufReader needs ownership, but we still need to write back on
        // the same socket — clone the file descriptor for reading.
        let read = stream.try_clone()?;
        let mut write = stream;
        self.handle_io(read, &mut write)
    }

    /// Handle one connection over arbitrary `Read` + `Write` halves.
    /// Used by the TLS path to wrap a `rustls::StreamOwned` and reuse the
    /// same dispatch logic.
    pub fn handle_io<R: Read, W: Write>(
        &self,
        reader: R,
        writer: &mut W,
    ) -> Result<(), ServerError> {
        let (req, body) = parse_request(reader)?;
        let mut outcome = DispatchOutcome::default();
        let dispatch_result = self.dispatch(&req, &body, writer, &mut outcome);
        let _ = writer.flush();
        // Audit AFTER response is flushed; failures here don't break the request.
        let principal = if outcome.principal.is_empty() {
            if self.auth_token.is_none() { "anonymous" } else { "unknown" }
        } else {
            outcome.principal.as_str()
        };
        self.record_audit(
            principal,
            &req.method,
            req.path_no_query(),
            outcome.status,
            outcome.tx_id,
            outcome.failure.as_deref(),
        );
        dispatch_result
    }

    fn dispatch(
        &self,
        req: &Request,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let path_no_q = req.path_no_query();
        let needed = required_capability(&req.method, path_no_q);

        // /health and unmatched paths bypass auth — let dispatch route them
        // to a 200 or 404 respectively.
        let needs_auth = needed.is_some() && needed != Some(Capability::Health);

        if needs_auth {
            // Principals-mode takes precedence over single-token-mode.
            if let Some(reg) = &self.principals {
                if req.bearer.is_empty() {
                    outcome.status = 401;
                    outcome.failure = Some("missing bearer token".into());
                    return write_error(out, 401, "unauthorized", "missing bearer token");
                }
                match reg.resolve(&req.bearer) {
                    None => {
                        outcome.status = 401;
                        outcome.failure = Some("unknown bearer token".into());
                        return write_error(out, 401, "unauthorized", "unknown bearer token");
                    }
                    Some(p) => {
                        outcome.principal.clone_from(&p.name);
                        if let Some(cap) = needed
                            && !p.allows(cap)
                        {
                            outcome.status = 403;
                            let detail = format!(
                                "principal '{}' lacks capability '{}'",
                                p.name,
                                capability_str(cap),
                            );
                            outcome.failure = Some(detail.clone());
                            return write_error(out, 403, "forbidden", &detail);
                        }
                    }
                }
            } else if let Some(expected) = &self.auth_token {
                if !constant_time_eq(expected.as_bytes(), req.bearer.as_bytes()) {
                    outcome.status = 401;
                    outcome.failure = Some("missing or invalid bearer token".into());
                    return write_error(
                        out,
                        401,
                        "unauthorized",
                        "missing or invalid bearer token",
                    );
                }
                outcome.principal = principal_for_token(&req.bearer);
            }
        }

        // For routes that accept query parameters, extract them from the
        // full request path (path_no_q strips them).
        let full_path = req.path.as_str();
        match (req.method.as_str(), path_no_q) {
            ("GET", "/health") => {
                outcome.status = 200;
                write_json(out, 200, &serde_json::json!({"status": "ok"}))
            }
            ("POST", "/commit") => self.handle_commit(body, out, outcome),
            ("GET", path) if path.starts_with("/read/") => {
                let after_prefix = &full_path["/read/".len()..];
                self.handle_read(after_prefix, out, outcome)
            }
            ("GET", "/iter") => {
                let query = full_path.split_once('?').map(|(_, q)| q);
                self.handle_iter(query, out, outcome)
            }
            ("POST", "/flush") => self.handle_flush(out, outcome),
            ("POST", "/compact") => self.handle_compact(out, outcome),
            ("POST", "/lookup") => self.handle_lookup(body, out, outcome),
            ("POST", "/vector_search") => self.handle_vector_search(body, out, outcome),
            ("POST", "/property_lookup") => self.handle_property_lookup(body, out, outcome),
            ("POST", "/property_range") => self.handle_property_range(body, out, outcome),
            ("POST", "/traverse") => self.handle_traverse(body, out, outcome),
            ("POST", "/query") => self.handle_query(body, out, outcome),
            ("POST", "/query_stream") => self.handle_query_stream(body, out, outcome),
            ("POST", "/subscribe") => self.handle_subscribe(body, out, outcome),
            _ => {
                outcome.status = 404;
                let detail = format!("no route for {} {}", req.method, req.path);
                outcome.failure = Some(detail.clone());
                write_error(out, 404, "not_found", &detail)
            }
        }
    }

    fn handle_flush(
        &self,
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        engine.flush()?;
        let (records, bytes) = engine.memtable_stats();
        outcome.status = 200;
        write_json(
            out,
            200,
            &serde_json::json!({
                "memtable_records": records,
                "memtable_bytes": bytes,
                "sstable_count": engine.sstable_count(),
            }),
        )
    }

    fn handle_compact(
        &self,
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let stats = engine.compact()?;
        outcome.status = 200;
        write_json(
            out,
            200,
            &serde_json::json!({
                "records_in": stats.records_in,
                "records_out": stats.records_out,
                "sstables_in": stats.sstables_in,
                "new_sstable_seq": stats.new_sstable_seq,
            }),
        )
    }

    fn handle_commit(
        &self,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let req: CommitRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => {
                outcome.status = 400;
                let detail = format!("commit body: {e}");
                outcome.failure = Some(detail.clone());
                return write_error(out, 400, "bad_json", &detail);
            }
        };
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let mut txn: WriteTxn = engine.begin_write();
        for jr in req.records {
            let r: Record = match jr.try_into() {
                Ok(r) => r,
                Err(e) => {
                    drop(txn); // rollback
                    outcome.status = 400;
                    outcome.failure = Some(e.to_string());
                    return write_error(out, 400, "bad_record", &e.to_string());
                }
            };
            stamp_and_push(&mut txn, r);
        }
        match txn.commit() {
            Ok(tid) => {
                // Drop the engine lock BEFORE notifying so subscribers
                // can grab the lock to read newly-committed records
                // without contending with the writer's still-held mutex.
                drop(engine);
                let (mu, cv) = &*self.commit_notify;
                {
                    let mut guard = mu.lock().expect("notify mutex poisoned");
                    if tid.get() > *guard {
                        *guard = tid.get();
                    }
                }
                cv.notify_all();
                outcome.status = 200;
                outcome.tx_id = Some(tid.get());
                write_json(out, 200, &CommitResponse { tx_id: tid.get() })
            }
            Err(EngineError::Validation(v)) => {
                outcome.status = 422;
                outcome.failure = Some(v.to_string());
                write_error(out, 422, "validation", &v.to_string())?;
                Ok(())
            }
            Err(e) => {
                outcome.status = 500;
                outcome.failure = Some(e.to_string());
                write_error(out, 500, "engine", &e.to_string())
            }
        }
    }

    fn handle_read(
        &self,
        uuid_and_query: &str,
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let (uuid_str, query) = split_path_query(uuid_and_query);
        let Ok(uuid) = uuid::Uuid::parse_str(uuid_str) else {
            outcome.status = 400;
            outcome.failure = Some(uuid_str.to_owned());
            return write_error(out, 400, "bad_uuid", uuid_str);
        };
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let snapshot = match resolve_snapshot_param(&engine, query) {
            Ok(s) => s,
            Err(detail) => return bad_request(out, outcome, "bad_snapshot_param", &detail),
        };
        let resolved = engine.snapshot_read(&uuid, snapshot)?;
        let body = match resolved {
            Resolved::Missing => ReadResponse::Missing,
            Resolved::Deleted { deleted_at } => ReadResponse::Deleted {
                deleted_at: deleted_at.get(),
            },
            Resolved::Live(r) => ReadResponse::Live {
                record: (&r).into(),
            },
        };
        outcome.status = 200;
        write_json(out, 200, &body)
    }

    fn handle_iter(
        &self,
        query: Option<&str>,
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let snapshot = match resolve_snapshot_param(&engine, query) {
            Ok(s) => s,
            Err(detail) => return bad_request(out, outcome, "bad_snapshot_param", &detail),
        };
        let records = engine.snapshot_iter(snapshot)?;
        // Write status + headers manually so we can stream JSONL.
        write_status_line(out, 200)?;
        out.write_all(b"Content-Type: application/jsonl; charset=utf-8\r\n")?;
        out.write_all(b"Connection: close\r\n\r\n")?;
        for r in records {
            // Filter internal v2.0 metadata records (TxTimestamp,
            // RetentionPolicy). Clients access these through dedicated
            // engine APIs, not /iter.
            if matches!(r, Record::TxTimestamp(_) | Record::RetentionPolicy(_)) {
                continue;
            }
            let jr: JsonRecord = (&r).into();
            let line = serde_json::to_string(&jr).map_err(|e| {
                ServerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })?;
            out.write_all(line.as_bytes())?;
            out.write_all(b"\n")?;
        }
        outcome.status = 200;
        Ok(())
    }

    fn handle_lookup(
        &self,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let req: LookupRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => return bad_json(out, outcome, "lookup body", &e.to_string()),
        };
        let value: Value = match req.value.try_into() {
            Ok(v) => v,
            Err(e) => return bad_request(out, outcome, "bad_value", &e),
        };
        let engine = self.engine.lock().expect("engine mutex poisoned");
        let hit = engine.lookup_by_external_key(PropertyId::new(req.property_id), &value);
        outcome.status = 200;
        write_json(
            out,
            200,
            &LookupResponse {
                entity_id: hit.map(|eid| eid.into_uuid().to_string()),
            },
        )
    }

    fn handle_vector_search(
        &self,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let req: VectorSearchRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => return bad_json(out, outcome, "vector_search body", &e.to_string()),
        };
        if req.k == 0 || req.k > MAX_VECTOR_K {
            outcome.status = 400;
            let detail = format!("k must be in 1..={MAX_VECTOR_K} (got {})", req.k);
            outcome.failure = Some(detail.clone());
            return write_error(out, 400, "bad_k", &detail);
        }
        let metric = match req.metric {
            VectorMetric::L2 => Distance::L2Squared,
            VectorMetric::Cosine => Distance::Cosine,
        };
        let engine = self.engine.lock().expect("engine mutex poisoned");
        let hits = engine.vector_search(PropertyId::new(req.property_id), &req.query, req.k, metric);
        let resp = VectorSearchResponse {
            hits: hits
                .into_iter()
                .map(|(eid, d)| VectorHit {
                    entity_id: eid.into_uuid().to_string(),
                    distance: d,
                })
                .collect(),
        };
        outcome.status = 200;
        write_json(out, 200, &resp)
    }

    fn handle_property_lookup(
        &self,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let req: PropertyLookupRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => return bad_json(out, outcome, "property_lookup body", &e.to_string()),
        };
        let value: Value = match req.value.try_into() {
            Ok(v) => v,
            Err(e) => return bad_request(out, outcome, "bad_value", &e),
        };
        let engine = self.engine.lock().expect("engine mutex poisoned");
        let hits = engine.property_lookup(
            TypeId::new(req.type_id),
            PropertyId::new(req.property_id),
            &value,
        );
        let resp = PropertyLookupResponse {
            entity_ids: hits.into_iter().map(|eid| eid.into_uuid().to_string()).collect(),
        };
        outcome.status = 200;
        write_json(out, 200, &resp)
    }

    fn handle_property_range(
        &self,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let req: PropertyRangeRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => return bad_json(out, outcome, "property_range body", &e.to_string()),
        };
        let low: Option<Value> = match req.low.map(JsonValue::try_into).transpose() {
            Ok(v) => v,
            Err(e) => {
                return bad_request(out, outcome, "bad_low", &WireError::to_string(&e));
            }
        };
        let high: Option<Value> = match req.high.map(JsonValue::try_into).transpose() {
            Ok(v) => v,
            Err(e) => {
                return bad_request(out, outcome, "bad_high", &WireError::to_string(&e));
            }
        };
        let engine = self.engine.lock().expect("engine mutex poisoned");
        let hits = engine.property_range(
            TypeId::new(req.type_id),
            PropertyId::new(req.property_id),
            low.as_ref(),
            high.as_ref(),
        );
        let resp = PropertyRangeResponse {
            entity_ids: hits.into_iter().map(|eid| eid.into_uuid().to_string()).collect(),
        };
        outcome.status = 200;
        write_json(out, 200, &resp)
    }

    /// `POST /traverse` — server-side BFS across N hops of hyperedges,
    /// returning every entity reachable at the final hop.
    ///
    /// Implementation: per-hop frontier expansion using the adjacency
    /// index (entity → incident hyperedge IDs) and the primary store
    /// (hyperedge ID → role bindings). Cycles are deduplicated via a
    /// visited set.
    fn handle_traverse(
        &self,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let req: TraverseRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => return bad_json(out, outcome, "traverse body", &e.to_string()),
        };
        let Ok(start_uuid) = uuid::Uuid::parse_str(&req.start) else {
            return bad_request(out, outcome, "bad_uuid", &req.start);
        };
        let start = EntityId::from_uuid(start_uuid);
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let snapshot = TxId::new(engine.manifest().last_tx_id);

        let mut frontier: std::collections::HashSet<EntityId> =
            std::collections::HashSet::from([start]);
        for hop in &req.hops {
            let mut next: std::collections::HashSet<EntityId> =
                std::collections::HashSet::new();
            for &current in &frontier {
                // Pull every hyperedge incident on `current`. The
                // adjacency index returns IDs; we read each to get role
                // bindings.
                for he_id in engine.hyperedges_for_entity(current) {
                    let resolved = engine.snapshot_read(&he_id.into_uuid(), snapshot)?;
                    let Resolved::Live(live) = resolved else {
                        continue;
                    };
                    let Record::HyperEdge(he) = live else {
                        continue;
                    };
                    if let Some(t) = hop.hyperedge_type_id
                        && he.type_id.get() != t
                    {
                        continue;
                    }
                    for (_role, eid) in &he.roles {
                        if *eid == current {
                            continue;
                        }
                        next.insert(*eid);
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        let resp = TraverseResponse {
            entity_ids: frontier
                .into_iter()
                .map(|e| e.into_uuid().to_string())
                .collect(),
        };
        outcome.status = 200;
        write_json(out, 200, &resp)
    }

    /// `POST /query` — execute a structured wire-AST query and return
    /// the result rows. The body is a `QueryRequest` (id-based AST); the
    /// resolver step (text → AST + name → id) is the caller's job. See
    /// the query-language working spec §4 for the request shape and §5
    /// for execution semantics.
    fn handle_query(
        &self,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let req: QueryRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => return bad_json(out, outcome, "query body", &e.to_string()),
        };
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let resp = match execute_query(&mut engine, req) {
            Ok(r) => r,
            Err(e) => return query_error_to_http(out, outcome, &e),
        };
        outcome.status = 200;
        write_json(out, 200, &resp)
    }

    /// `POST /query_stream` — same semantics as `/query` but the response
    /// is streamed as JSONL (one row per line) instead of materialised
    /// in a single JSON body. Useful for large result sets where the
    /// client wants to consume rows incrementally.
    ///
    /// The first line emitted is the header
    /// `{"columns": [...], "truncated": <bool>}`; every subsequent line
    /// is one row, an array of `JsonValue`s in column order. End of
    /// stream is the closed connection (no trailing line).
    ///
    /// v1 caveat: the engine executor still materialises all binding
    /// rows in memory before this route streams them. End-to-end lazy
    /// execution lands in v2 once the executor is rewritten as an
    /// iterator pipeline.
    fn handle_query_stream(
        &self,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let req: QueryRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => return bad_json(out, outcome, "query body", &e.to_string()),
        };
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let resp = match execute_query(&mut engine, req) {
            Ok(r) => r,
            Err(e) => return query_error_to_http(out, outcome, &e),
        };
        write_status_line(out, 200)?;
        out.write_all(b"Content-Type: application/jsonl; charset=utf-8\r\n")?;
        out.write_all(b"Connection: close\r\n\r\n")?;
        // First line: header.
        let header = serde_json::json!({
            "columns": resp.columns,
            "truncated": resp.truncated,
        });
        let header_line = serde_json::to_string(&header).map_err(|e| {
            ServerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        out.write_all(header_line.as_bytes())?;
        out.write_all(b"\n")?;
        // Subsequent lines: one row each.
        for row in &resp.rows {
            let line = serde_json::to_string(row).map_err(|e| {
                ServerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })?;
            out.write_all(line.as_bytes())?;
            out.write_all(b"\n")?;
        }
        outcome.status = 200;
        Ok(())
    }

    /// `POST /subscribe` — long-poll for records committed after a
    /// given tx_id. Returns JSONL: first line is
    /// `{"current_tx_id": <N>}`, subsequent lines are each newly-visible
    /// record (one JsonRecord per line). End of stream is closed
    /// connection. If no commits arrive before `timeout_ms`, returns
    /// only the header line with `current_tx_id == since_tx_id`.
    ///
    /// v2.0: condvar-based — `/commit` fires `notify_all` on the
    /// server's `commit_notify` after every successful commit. This
    /// handler blocks on `wait_timeout_while` and returns the moment a
    /// later tx_id is observed. Sub-millisecond latency under low load.
    ///
    /// v2.1 caveat: the bounded test server (`serve_n`) and the
    /// production loop (`serve`) both process connections sequentially.
    /// While `/subscribe` is blocked in the condvar wait, the server
    /// can't accept the `/commit` connection that would fire the
    /// notify. Real sub-millisecond latency requires the server to
    /// spawn-per-connection, which is a v2.1 deliverable. The condvar
    /// machinery is correct; only the connection acceptor is the
    /// bottleneck. See `subscribe_wakes_on_concurrent_commit_within_a_millisecond_class_latency`
    /// in the test suite (marked `#[ignore]` pending v2.1).
    fn handle_subscribe(
        &self,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let req: SubscribeRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => return bad_json(out, outcome, "subscribe body", &e.to_string()),
        };
        let server_max_ms: u32 = 60_000;
        let timeout_ms = req.timeout_ms.unwrap_or(30_000).min(server_max_ms);
        let timeout = std::time::Duration::from_millis(u64::from(timeout_ms));

        // Cheap pre-check via the engine manifest: if commits have already
        // landed past since_tx_id (e.g., from direct engine.begin_write
        // calls in tests, or from a prior /commit we missed notifying for
        // any reason), return immediately. Manifest is ground truth;
        // commit_notify is a wake hint.
        let manifest_tx = {
            let e = self.engine.lock().expect("engine mutex poisoned");
            e.manifest().last_tx_id
        };
        let cur_tx = if manifest_tx > req.since_tx_id {
            manifest_tx
        } else {
            // Block on the condvar until a /commit fires notify_all OR
            // the timeout elapses. Re-check the manifest after each wake
            // so we tolerate notifications that race ahead.
            let (mu, cv) = &*self.commit_notify;
            let guard = mu.lock().expect("notify mutex poisoned");
            let (final_guard, _wait_result) = cv
                .wait_timeout_while(guard, timeout, |latest| *latest <= req.since_tx_id)
                .expect("condvar wait failed");
            // Re-read the manifest in case the condvar guard lags
            // (a separate writer could have committed without going
            // through /commit's notify hook).
            let post = {
                let e = self.engine.lock().expect("engine mutex poisoned");
                e.manifest().last_tx_id
            };
            post.max(*final_guard)
        };

        // Stream the response: header line, then records committed
        // after since_tx_id.
        write_status_line(out, 200)?;
        out.write_all(b"Content-Type: application/jsonl; charset=utf-8\r\n")?;
        out.write_all(b"Connection: close\r\n\r\n")?;
        let header = serde_json::json!({ "current_tx_id": cur_tx });
        let header_line = serde_json::to_string(&header).map_err(|e| {
            ServerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        out.write_all(header_line.as_bytes())?;
        out.write_all(b"\n")?;

        if cur_tx > req.since_tx_id {
            let mut engine = self.engine.lock().expect("engine mutex poisoned");
            let records = engine.snapshot_iter(TxId::new(cur_tx))?;
            for r in records {
                // Skip internal v2.0 metadata records. Subscribers get
                // user-facing data only.
                if matches!(r, Record::TxTimestamp(_) | Record::RetentionPolicy(_)) {
                    continue;
                }
                let assert_tx = match &r {
                    Record::Entity(e) => e.tx_id_assert.get(),
                    Record::HyperEdge(h) => h.tx_id_assert.get(),
                    // Dictionary records: emit on every subscribe so
                    // new schema entries reach subscribers.
                    _ => req.since_tx_id + 1,
                };
                if assert_tx <= req.since_tx_id {
                    continue;
                }
                let jr: JsonRecord = (&r).into();
                let line = serde_json::to_string(&jr).map_err(|e| {
                    ServerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                })?;
                out.write_all(line.as_bytes())?;
                out.write_all(b"\n")?;
            }
        }
        outcome.status = 200;
        Ok(())
    }

    /// Borrow the shared engine for direct manipulation (tests).
    #[must_use]
    pub fn engine(&self) -> Arc<Mutex<Engine>> {
        Arc::clone(&self.engine)
    }
}

/// Split a path-with-query like `01923c.../?snapshot=42` into
/// `("01923c...", Some("snapshot=42"))`. The path is everything up to the
/// first `?`; the query is everything after (or `None` if absent).
fn split_path_query(s: &str) -> (&str, Option<&str>) {
    match s.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (s, None),
    }
}

/// Resolve the snapshot tx_id for a request that accepts `?snapshot=N`
/// (specific tx_id) OR `?timestamp_us=T` (latest tx at or before T) as
/// query parameters. Missing query → latest committed tx.
///
/// Specifying both `snapshot` and `timestamp_us` is rejected to avoid
/// ambiguity. Unknown keys are ignored.
fn resolve_snapshot_param(
    engine: &Engine,
    query: Option<&str>,
) -> Result<TxId, String> {
    let mut tx_id: Option<u64> = None;
    let mut timestamp_us: Option<i64> = None;
    if let Some(q) = query {
        for kv in q.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            match k {
                "snapshot" => {
                    tx_id = Some(v.parse().map_err(|_| format!("bad snapshot={v}"))?);
                }
                "timestamp_us" => {
                    timestamp_us = Some(v.parse().map_err(|_| format!("bad timestamp_us={v}"))?);
                }
                _ => {}
            }
        }
    }
    match (tx_id, timestamp_us) {
        (Some(_), Some(_)) => Err("specify either snapshot or timestamp_us, not both".into()),
        (Some(n), None) => Ok(TxId::new(n)),
        (None, Some(ts)) => engine
            .tx_at_or_before(ts)
            .ok_or_else(|| format!("no tx_id at or before timestamp_us={ts}")),
        (None, None) => Ok(TxId::new(engine.manifest().last_tx_id)),
    }
}

/// Map a `QueryError` into the right HTTP status + error code. Codes are
/// kept identical to the engine-side names so clients can switch on them
/// without a translation table.
fn query_error_to_http(
    out: &mut dyn Write,
    outcome: &mut DispatchOutcome,
    err: &QueryError,
) -> Result<(), ServerError> {
    let (status, code) = match err {
        QueryError::Engine(_) => (500, "engine_error"),
        QueryError::RecursionConfigInvalid { .. } => (400, "recursion_config_invalid"),
        QueryError::RecursionDepthExceeded { .. } => (400, "recursion_depth_exceeded"),
        QueryError::TimestampUnavailable { .. } => (410, "timestamp_unavailable"),
        QueryError::SnapshotUnavailable { .. } => (410, "snapshot_unavailable"),
        QueryError::TypeNotIndexed { .. } => (400, "type_not_indexed"),
        QueryError::UnboundVariableAtExec { .. } => (400, "unbound_variable_at_exec"),
    };
    outcome.status = status;
    let detail = err.to_string();
    outcome.failure = Some(detail.clone());
    write_error(out, status, code, &detail)
}

/// Build a rustls `ServerConfig` from PEM-encoded cert chain + PKCS#8 key.
fn build_rustls_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<rustls::ServerConfig, ServerError> {
    use rustls_pemfile::Item;
    let cert_bytes = std::fs::read(cert_path)?;
    let mut cert_reader = std::io::BufReader::new(cert_bytes.as_slice());
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(ServerError::BadRequest("no PEM certificates found"));
    }
    let key_bytes = std::fs::read(key_path)?;
    let mut key_reader = std::io::BufReader::new(key_bytes.as_slice());
    let key = loop {
        match rustls_pemfile::read_one(&mut key_reader)
            .map_err(|e| ServerError::Io(std::io::Error::other(e)))?
        {
            Some(Item::Pkcs8Key(k)) => {
                break rustls::pki_types::PrivateKeyDer::Pkcs8(k);
            }
            Some(Item::Pkcs1Key(k)) => break rustls::pki_types::PrivateKeyDer::Pkcs1(k),
            Some(Item::Sec1Key(k)) => break rustls::pki_types::PrivateKeyDer::Sec1(k),
            Some(_) => {}
            None => {
                return Err(ServerError::BadRequest("no private key found"));
            }
        }
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| {
            ServerError::Io(std::io::Error::other(format!("rustls protocol error: {e}")))
        })?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| {
            ServerError::Io(std::io::Error::other(format!(
                "rustls server cert error: {e}"
            )))
        })?;
    Ok(cfg)
}

/// Server bound to an address; useful for tests that pick port 0.
pub struct BoundServer<'a> {
    /// Reference back to the server.
    pub server: &'a Server,
    listener: TcpListener,
}

impl BoundServer<'_> {
    /// Local address (with concrete port if 0 was supplied).
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept and serve forever.
    pub fn serve(&self) -> Result<(), ServerError> {
        for stream in self.listener.incoming() {
            match stream {
                Ok(s) => {
                    if let Err(e) = self.server.handle_connection(s) {
                        eprintln!("connection error: {e}");
                    }
                }
                Err(e) => eprintln!("accept error: {e}"),
            }
        }
        Ok(())
    }

    /// Accept and serve N connections, then return. Used by tests.
    pub fn serve_n(&self, n: usize) -> Result<(), ServerError> {
        for _ in 0..n {
            let (stream, _addr) = self.listener.accept()?;
            if let Err(e) = self.server.handle_connection(stream) {
                eprintln!("connection error: {e}");
            }
        }
        Ok(())
    }
}

/// TLS-bound server. Same shape as [`BoundServer`] but wraps each
/// accepted `TcpStream` in a `rustls::ServerConnection` before dispatch.
pub struct BoundTlsServer<'a> {
    /// Reference back to the server.
    pub server: &'a Server,
    listener: TcpListener,
    cfg: Arc<rustls::ServerConfig>,
}

impl BoundTlsServer<'_> {
    /// Local address (with concrete port if 0 was supplied).
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    fn handle_one(&self, stream: TcpStream) -> Result<(), ServerError> {
        let conn = rustls::ServerConnection::new(Arc::clone(&self.cfg))
            .map_err(|e| ServerError::Io(std::io::Error::other(format!("rustls: {e}"))))?;
        // StreamOwned drives the TLS handshake + record layer transparently.
        let mut tls = rustls::StreamOwned::new(conn, stream);
        // Split borrow: the same stream is both reader and writer. We read
        // headers + body up-front via BufReader (owning a &mut to tls), then
        // write directly back through tls afterwards.
        let (req, body) = {
            let r = &mut tls;
            parse_request(r)?
        };
        let mut outcome = DispatchOutcome::default();
        let dispatch_result = self.server.dispatch(&req, &body, &mut tls, &mut outcome);
        let _ = tls.flush();
        let principal = if outcome.principal.is_empty() {
            if self.server.auth_token.is_none() && self.server.principals.is_none() {
                "anonymous"
            } else {
                "unknown"
            }
        } else {
            outcome.principal.as_str()
        };
        self.server.record_audit(
            principal,
            &req.method,
            req.path_no_query(),
            outcome.status,
            outcome.tx_id,
            outcome.failure.as_deref(),
        );
        dispatch_result
    }

    /// Accept and serve forever.
    pub fn serve(&self) -> Result<(), ServerError> {
        for stream in self.listener.incoming() {
            match stream {
                Ok(s) => {
                    if let Err(e) = self.handle_one(s) {
                        eprintln!("tls connection error: {e}");
                    }
                }
                Err(e) => eprintln!("tls accept error: {e}"),
            }
        }
        Ok(())
    }

    /// Accept and serve N connections, then return.
    pub fn serve_n(&self, n: usize) -> Result<(), ServerError> {
        for _ in 0..n {
            let (stream, _addr) = self.listener.accept()?;
            if let Err(e) = self.handle_one(stream) {
                eprintln!("tls connection error: {e}");
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hand-rolled HTTP/1.1 parsing + response writing
// ---------------------------------------------------------------------------

/// Parsed HTTP request head.
#[derive(Debug)]
struct Request {
    method: String,
    /// Includes the query string, if any.
    path: String,
    /// Raw `Authorization` header value (without the `Bearer ` prefix),
    /// or empty.
    bearer: String,
}

impl Request {
    fn path_no_query(&self) -> &str {
        self.path.split('?').next().unwrap_or(&self.path)
    }
}

fn parse_request<R: Read>(stream: R) -> Result<(Request, Vec<u8>), ServerError> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Err(ServerError::BadRequest("empty request"));
    }
    let mut parts = request_line.trim_end().split(' ');
    let method = parts
        .next()
        .ok_or(ServerError::BadRequest("no method"))?
        .to_owned();
    let path = parts
        .next()
        .ok_or(ServerError::BadRequest("no path"))?
        .to_owned();
    // Discard HTTP version token; we don't validate it.

    // Read headers until blank line.
    let mut content_length: usize = 0;
    let mut bearer = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(ServerError::BadRequest("eof in headers"));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if key.eq_ignore_ascii_case("content-length") {
                content_length = val.parse().unwrap_or(0);
            } else if key.eq_ignore_ascii_case("authorization") {
                // Strip optional "Bearer " (case-insensitive) prefix;
                // anything else stays as-is so future schemes can be
                // added without re-parsing.
                let token = val
                    .strip_prefix("Bearer ")
                    .or_else(|| val.strip_prefix("bearer "))
                    .unwrap_or(val);
                bearer.clear();
                bearer.push_str(token);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok((
        Request {
            method,
            path,
            bearer,
        },
        body,
    ))
}

/// Captures per-request metadata so the audit logger can write a row
/// once dispatch finishes, regardless of which handler ran.
#[derive(Debug, Default)]
struct DispatchOutcome {
    status: u16,
    principal: String,
    tx_id: Option<u64>,
    failure: Option<String>,
}

/// Map a `(method, path)` to the capability required to invoke it. Returns
/// `None` for routes the server doesn't recognise — those land in the 404
/// branch which is intentionally open.
fn required_capability(method: &str, path: &str) -> Option<Capability> {
    match (method, path) {
        ("GET", "/health") => Some(Capability::Health),
        ("POST", "/commit") => Some(Capability::Commit),
        ("GET", p) if p.starts_with("/read/") => Some(Capability::Read),
        ("GET", "/iter") => Some(Capability::Iter),
        ("POST", "/flush") => Some(Capability::Flush),
        ("POST", "/compact") => Some(Capability::Compact),
        // Indexed query + traversal + query-language + subscribe routes
        // — all gated by Read.
        (
            "POST",
            "/lookup" | "/vector_search" | "/property_lookup" | "/property_range" | "/traverse"
                | "/query" | "/query_stream" | "/subscribe",
        ) => Some(Capability::Read),
        _ => None,
    }
}

fn bad_json(
    out: &mut dyn Write,
    outcome: &mut DispatchOutcome,
    context: &str,
    detail: &str,
) -> Result<(), ServerError> {
    outcome.status = 400;
    let combined = format!("{context}: {detail}");
    outcome.failure = Some(combined.clone());
    write_error(out, 400, "bad_json", &combined)
}

fn bad_request<E: std::fmt::Display>(
    out: &mut dyn Write,
    outcome: &mut DispatchOutcome,
    code: &str,
    err: &E,
) -> Result<(), ServerError> {
    let detail = err.to_string();
    outcome.status = 400;
    outcome.failure = Some(detail.clone());
    write_error(out, 400, code, &detail)
}

fn capability_str(c: Capability) -> &'static str {
    match c {
        Capability::Health => "health",
        Capability::Read => "read",
        Capability::Iter => "iter",
        Capability::Commit => "commit",
        Capability::Flush => "flush",
        Capability::Compact => "compact",
        Capability::Admin => "admin",
    }
}

/// Stable, short identifier for a bearer token. v1 uses an 8-char prefix
/// of the SHA256-equivalent: a simple deterministic non-reversible hash
/// to avoid logging the raw token. (For a single-token deployment this
/// is just a constant — fine; for multi-principal v2 each principal
/// hashes to a distinct prefix.)
fn principal_for_token(token: &str) -> String {
    // FNV-1a 64-bit — small, dep-free, good enough for "stable identifier".
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in token.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("token:{h:016x}")
}

/// Constant-time string compare so a malicious caller can't time-side-channel
/// the token byte-by-byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn status_text(code: u16) -> &'static str {
    match code {
        400 => "Bad Request",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        // Default reason phrase for 200 + anything unrecognised.
        _ => "OK",
    }
}

fn write_status_line(out: &mut dyn Write, code: u16) -> std::io::Result<()> {
    write!(out, "HTTP/1.1 {code} {}\r\n", status_text(code))
}

fn write_json<T: Serialize>(out: &mut dyn Write, code: u16, body: &T) -> Result<(), ServerError> {
    let bytes = serde_json::to_vec(body)
        .map_err(|e| ServerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    write_status_line(out, code)?;
    write!(
        out,
        "Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        bytes.len()
    )?;
    out.write_all(&bytes)?;
    Ok(())
}

fn write_error(out: &mut dyn Write, code: u16, err: &str, detail: &str) -> Result<(), ServerError> {
    let body = ErrorResponse {
        error: err.to_owned(),
        detail: detail.to_owned(),
    };
    write_json(out, code, &body)
}

fn stamp_and_push(txn: &mut WriteTxn<'_>, r: Record) {
    match r {
        Record::Entity(e) => txn.put_entity(e),
        Record::HyperEdge(h) => txn.put_hyperedge(h),
        Record::Tombstone(t) => txn.delete(t.target_id),
        // Dictionary records are forwarded verbatim; the engine does
        // not gate them currently. v2 may decide that dictionary
        // entries are admin-only and reject non-admin commits.
        other => txn.put_raw(other),
    }
}
