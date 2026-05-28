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
use std::sync::{Arc, Mutex, RwLock};
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
///
/// v2.1: when `entity_id` is `Some(_)`, this principal lives in the
/// engine as a `TYPE_PRINCIPAL` entity + N `TYPE_CAPABILITY` hyperedges;
/// the dispatch path calls `Engine::has_capability` on every request,
/// so capability revocations via `/commit` become effective without a
/// server restart. When `entity_id` is `None` (legacy callers that built
/// `Principal` directly via `with_principals`), the dispatch falls back
/// to `Principal::allows` over the in-memory `capabilities` set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Principal {
    /// Stable display name (used in audit logs and error messages).
    pub name: String,
    /// Direct capability grants. v2.1: snapshot only — the dispatch
    /// path reads the engine when `entity_id` is set. Still populated
    /// for diagnostics + the legacy fall-back path.
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
    /// Engine-side entity id for this principal. `Some` when the
    /// principal was loaded via `with_principals_bootstrapped`; `None`
    /// for principals supplied directly via `with_principals(...)`.
    /// In-memory only — never round-tripped through the JSON config.
    #[serde(skip)]
    pub entity_id: Option<ndb_engine::EntityId>,
}

impl Principal {
    /// True iff this principal can perform `cap` (either directly or via
    /// the `Admin` wildcard). Used only as the fall-back when
    /// `entity_id` is `None`; engine-backed principals go through
    /// `Engine::has_capability` instead.
    #[must_use]
    pub fn allows(&self, cap: Capability) -> bool {
        self.capabilities.contains(&Capability::Admin) || self.capabilities.contains(&cap)
    }
}

impl Capability {
    /// Canonical string used as the `action` property on the matching
    /// capability hyperedge in the engine. Stable contract — engine
    /// records persisted under these names; renames would silently
    /// invalidate every imported capability.
    #[must_use]
    pub fn as_action(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::Read => "read",
            Self::Iter => "iter",
            Self::Commit => "commit",
            Self::Flush => "flush",
            Self::Compact => "compact",
            Self::Admin => ndb_engine::WILDCARD,
        }
    }

    /// Parse an engine-stored action string back into the enum. Returns
    /// `None` for unknown actions (forward-compat — a v3 capability
    /// stored in the database that this binary doesn't recognise
    /// shouldn't blow up the auth flow).
    #[must_use]
    pub fn from_action(s: &str) -> Option<Self> {
        match s {
            "health" => Some(Self::Health),
            "read" => Some(Self::Read),
            "iter" => Some(Self::Iter),
            "commit" => Some(Self::Commit),
            "flush" => Some(Self::Flush),
            "compact" => Some(Self::Compact),
            s if s == ndb_engine::WILDCARD => Some(Self::Admin),
            _ => None,
        }
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
///
/// As of v3-final the engine is held behind `RwLock<Engine>` (was
/// `Mutex<Engine>` through v1.3). Every read route — `/read`, `/iter`,
/// `/lookup`, `/property_lookup`, `/property_range`, `/vector_search`,
/// `/traverse`, `/query`/`/query_text` for read-only queries — takes
/// `.read()` and parallelises. Write routes (`/commit`, `/flush`,
/// `/compact`, query with `create`/`set`/`merge`/`delete` clauses) take
/// `.write()` and serialise. The single-writer engine model is
/// preserved by the RwLock's writer slot; concurrent reads parallelise
/// up to the host CPU.
pub struct Server {
    engine: Arc<RwLock<Engine>>,
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
    /// v2.2 preview: when set, every response carries
    /// `Access-Control-Allow-Origin: <value>` and `OPTIONS` preflight
    /// requests get a 204 with the matching ACAO. Use `"*"` to allow
    /// any origin. Designed for the local `ndb-explorer` SPA; not
    /// for production-internet exposure (use a reverse proxy with
    /// per-origin policy for that).
    cors_origin: Option<String>,
    /// When `true`, every mutating route returns 403 — the server is a
    /// pure-read surface. Used by public demo servers exposed via the
    /// knowledge-site proxy so a visitor's `/query/text` can't delete
    /// or mutate the demo database. Mutating routes: `/commit`,
    /// `/flush`, `/compact`, and any `/query[/text]` whose body has
    /// `creates` / `deletes` / `sets` / `merges`.
    read_only: bool,
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

/// Commit every principal in `p` to the engine as one principal entity
/// plus one capability hyperedge per granted capability. Returns the
/// number of capability hyperedges committed.
fn commit_principals_to_engine(
    engine: &mut Engine,
    p: &Principals,
) -> Result<usize, ServerError> {
    use ndb_engine::{
        EntityRecord, HyperEdgeRecord, PROP_ACTION, PROP_EXPIRES_AT, PROP_GRANTED_AT,
        PROP_PRINCIPAL_NAME, PROP_PRINCIPAL_TOKEN, PROP_TARGET, ROLE_SUBJECT, TYPE_CAPABILITY,
        TYPE_PRINCIPAL, Value, WILDCARD,
    };
    let now_us = i64::try_from(now_micros()).unwrap_or(0);
    let mut n_caps = 0usize;
    for (token, principal) in &p.principals {
        let mut txn = engine.begin_write();
        let principal_eid = ndb_engine::EntityId::now_v7();
        let tx_id = txn.tx_id();
        txn.put_entity(EntityRecord {
            entity_id: principal_eid,
            type_id: TYPE_PRINCIPAL,
            tx_id_assert: tx_id,
            tx_id_supersede: TxId::ACTIVE,
            properties: vec![
                (PROP_PRINCIPAL_NAME, Value::String(principal.name.clone())),
                (PROP_PRINCIPAL_TOKEN, Value::String(token.clone())),
            ],
        });
        for cap in &principal.capabilities {
            txn.put_hyperedge(HyperEdgeRecord {
                hyperedge_id: ndb_engine::HyperedgeId::now_v7(),
                type_id: TYPE_CAPABILITY,
                tx_id_assert: tx_id,
                tx_id_supersede: TxId::ACTIVE,
                roles: vec![(ROLE_SUBJECT, principal_eid)],
                hyperedge_roles: Vec::new(),
                properties: vec![
                    (PROP_ACTION, Value::String(cap.as_action().into())),
                    (PROP_TARGET, Value::String(WILDCARD.into())),
                    (PROP_GRANTED_AT, Value::Timestamp(now_us)),
                    (PROP_EXPIRES_AT, Value::Timestamp(0)),
                ],
            });
            n_caps += 1;
        }
        txn.commit()?;
    }
    Ok(n_caps)
}

/// Rebuild the in-memory `Principals` cache from capability hyperedges
/// in the engine. Token → Principal mapping is reconstructed by joining
/// each principal entity's `PROP_PRINCIPAL_TOKEN` against the set of
/// capability hyperedges incident on that entity.
fn principals_from_engine(engine: &Arc<RwLock<Engine>>) -> Result<Principals, ServerError> {
    use ndb_engine::{
        PROP_ACTION, PROP_PRINCIPAL_NAME, PROP_PRINCIPAL_TOKEN, ROLE_SUBJECT, Record, TYPE_CAPABILITY,
        TYPE_PRINCIPAL, Value,
    };
    let eng = engine.read().expect("engine lock poisoned");
    let snapshot = TxId::new(eng.manifest().last_tx_id);
    // Step 1: gather principal entities — id, name, token.
    let mut principals_by_eid: HashMap<ndb_engine::EntityId, (String, String)> = HashMap::new();
    for rec in eng.snapshot_iter_streaming(snapshot) {
        let rec = rec?;
        if let Record::Entity(e) = rec
            && e.type_id == TYPE_PRINCIPAL
        {
            let mut name = None;
            let mut token = None;
            for (pid, val) in &e.properties {
                if *pid == PROP_PRINCIPAL_NAME
                    && let Value::String(s) = val
                {
                    name = Some(s.clone());
                }
                if *pid == PROP_PRINCIPAL_TOKEN
                    && let Value::String(s) = val
                {
                    token = Some(s.clone());
                }
            }
            if let (Some(n), Some(t)) = (name, token) {
                principals_by_eid.insert(e.entity_id, (n, t));
            }
        }
    }
    // Step 2: walk capability hyperedges, fold their actions into the
    // matching principal's BTreeSet.
    let mut by_token: HashMap<String, Principal> = HashMap::new();
    for (eid, (name, token)) in principals_by_eid {
        let mut caps: BTreeSet<Capability> = BTreeSet::new();
        for hid in eng.hyperedges_for_entity(eid) {
            let resolved = eng.snapshot_read(&hid.into_uuid(), snapshot)?;
            if let Resolved::Live(Record::HyperEdge(h)) = resolved
                && h.type_id == TYPE_CAPABILITY
                && h.roles.iter().any(|(rid, e)| *rid == ROLE_SUBJECT && *e == eid)
            {
                for (pid, val) in &h.properties {
                    if *pid == PROP_ACTION
                        && let Value::String(s) = val
                        && let Some(c) = Capability::from_action(s)
                    {
                        caps.insert(c);
                    }
                }
            }
        }
        by_token.insert(
            token,
            Principal {
                name,
                capabilities: caps,
                entity_id: Some(eid),
            },
        );
    }
    Ok(Principals {
        principals: by_token,
    })
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
            engine: Arc::new(RwLock::new(engine)),
            auth_token: None,
            principals: None,
            audit: None,
            tls_config: None,
            commit_notify: Arc::new((Mutex::new(initial_tx), std::sync::Condvar::new())),
            cors_origin: None,
            read_only: false,
        })
    }

    /// Wrap an already-opened engine. Useful for tests.
    #[must_use]
    pub fn from_engine(engine: Engine) -> Self {
        let initial_tx = engine.manifest().last_tx_id;
        Self {
            engine: Arc::new(RwLock::new(engine)),
            auth_token: None,
            principals: None,
            audit: None,
            tls_config: None,
            commit_notify: Arc::new((Mutex::new(initial_tx), std::sync::Condvar::new())),
            cors_origin: None,
            read_only: false,
        }
    }

    /// Lock the server into read-only mode. Mutating routes
    /// (`/commit`, `/flush`, `/compact`) return 403; mutating
    /// query clauses (`create`, `delete`, `set`, `merge`) inside
    /// `/query` or `/query/text` also return 403. The in-process
    /// `Engine` is untouched — seed loading + admin code paths run
    /// normally.
    #[must_use]
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// v2.2 preview: enable `Access-Control-Allow-Origin` headers on
    /// every response, plus `OPTIONS` preflight handling. Pass `"*"`
    /// to allow any origin (fine for localhost-only `ndb-explorer`
    /// usage). For production-internet exposure, terminate CORS at a
    /// reverse proxy instead.
    #[must_use]
    pub fn with_cors_origin(mut self, origin: impl Into<String>) -> Self {
        let v = origin.into();
        self.cors_origin = if v.is_empty() { None } else { Some(v) };
        self
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

    /// Read-only access to the installed principals registry. Returns
    /// an empty registry when none was installed. Intended for tests
    /// and operator inspection; production auth flows go through
    /// [`dispatch`](Self::dispatch).
    #[must_use]
    pub fn principals_for_test(&self) -> Principals {
        self.principals.clone().unwrap_or_default()
    }

    /// Convenience: look for `<db>/.principals.json` and install it if
    /// present. Returns `Ok(self, true)` if a file was loaded; `Ok(self,
    /// false)` if the file was absent (no change to the server); error on
    /// any other I/O or parse failure.
    pub fn with_principals_from_db(self) -> Result<(Self, bool), ServerError> {
        let dir = {
            let eng = self.engine.read().expect("engine lock poisoned");
            eng.path().to_path_buf()
        };
        let path = dir.join(PRINCIPALS_FILENAME);
        match Principals::load(&path)? {
            Some(p) => Ok((self.with_principals(p), true)),
            None => Ok((self, false)),
        }
    }

    /// v2.1 §2.2: rebuild the in-memory `token → principal` cache from
    /// the engine. Operators can call this after committing new
    /// principal/capability records to make the new tokens resolvable
    /// without a server restart. Existing tokens whose capability
    /// hyperedges changed don't need a refresh — the dispatch path
    /// queries the engine on every request.
    pub fn refresh_principals_cache(&mut self) -> Result<(), ServerError> {
        let p = principals_from_engine(&self.engine)?;
        self.principals = Some(p);
        Ok(())
    }

    /// v2.0 #25 entry point: persist the principals set as capability
    /// hyperedges in the engine, and serve auth from the engine on
    /// subsequent opens.
    ///
    /// Behaviour:
    /// - If the engine already has any capability hyperedge or principal
    ///   entity, do nothing on disk — just rebuild the in-memory cache
    ///   from engine queries.
    /// - Otherwise, if `<db>/.principals.json` exists, parse it +
    ///   commit one principal entity per token + one capability
    ///   hyperedge per (principal, capability) pair, then load the
    ///   cache from engine.
    /// - Otherwise, install an empty principals registry (= no auth
    ///   gating; same as the bare `with_principals_from_db` path).
    ///
    /// Returns `(self, n_imported)` — `n_imported` is the number of
    /// capability hyperedges committed during this call. Zero on a
    /// second open (engine already populated).
    pub fn with_principals_bootstrapped(self) -> Result<(Self, usize), ServerError> {
        let dir = {
            let eng = self.engine.read().expect("engine lock poisoned");
            eng.path().to_path_buf()
        };
        let mut n_imported = 0;
        // Inside one engine lock so the populate-then-read window can't
        // race a concurrent writer.
        {
            let mut eng = self.engine.write().expect("engine lock poisoned");
            if !eng.has_any_capability_or_principal()? {
                let path = dir.join(PRINCIPALS_FILENAME);
                if let Some(p) = Principals::load(&path)? {
                    n_imported = commit_principals_to_engine(&mut eng, &p)?;
                }
            }
        }
        let cache = principals_from_engine(&self.engine)?;
        Ok((self.with_principals(cache), n_imported))
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
            let eng = self.engine.read().expect("engine lock poisoned");
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
        // v2.2 CORS: wrap the writer so every response gets an
        // `Access-Control-Allow-Origin` header injected right after
        // the last response header. The injector buffers up to the
        // `\r\n\r\n` terminator, then passes everything else through —
        // streaming responses (/iter, /query_stream, /subscribe) keep
        // their streaming behaviour.
        let dispatch_result = if let Some(origin) = self.cors_origin.clone() {
            let mut inject = HeaderInjector::new(
                writer,
                format!("Access-Control-Allow-Origin: {origin}\r\n").into_bytes(),
            );
            let r = self.dispatch(&req, &body, &mut inject, &mut outcome);
            let _ = inject.flush();
            r
        } else {
            self.dispatch(&req, &body, writer, &mut outcome)
        };
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

    #[allow(clippy::too_many_lines)] // long match over routes is the natural shape
    fn dispatch(
        &self,
        req: &Request,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let path_no_q = req.path_no_query();

        // v2.2 CORS preflight: respond to OPTIONS with the configured
        // ACAO + the headers/methods the explorer SPA needs. Browsers
        // require a 2xx response before they'll send the real request.
        // Without CORS configured, OPTIONS falls through to a 404.
        if req.method == "OPTIONS"
            && let Some(origin) = &self.cors_origin
        {
            outcome.status = 204;
            return write_cors_preflight(out, origin);
        }

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
                        if let Some(cap) = needed {
                            // v2.1: engine-backed dispatch when the
                            // principal carries an `entity_id` (set by
                            // `with_principals_bootstrapped`). Revocations
                            // via `/commit` become effective on the next
                            // request — the engine query is authoritative.
                            //
                            // Legacy fallback (no entity_id): consult the
                            // in-memory capability set — same behaviour as
                            // v2.0.
                            let allowed = if let Some(eid) = p.entity_id {
                                let now_us =
                                    i64::try_from(now_micros()).unwrap_or(i64::MAX);
                                let eng = self.engine.read().expect("engine lock poisoned");
                                match eng.has_capability(eid, cap.as_action(), "*", now_us) {
                                    Ok(b) => b,
                                    Err(e) => {
                                        outcome.status = 500;
                                        outcome.failure = Some(format!("auth lookup: {e}"));
                                        return write_error(
                                            out,
                                            500,
                                            "internal",
                                            &format!("auth lookup failed: {e}"),
                                        );
                                    }
                                }
                            } else {
                                p.allows(cap)
                            };
                            if !allowed {
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
            ("POST", "/query/text") => self.handle_query_text(body, out, outcome),
            ("POST", "/query/explain") => self.handle_query_explain(body, out, outcome),
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
        if self.read_only { return reject_read_only(out, outcome, "/flush"); }
        let mut engine = self.engine.write().expect("engine lock poisoned");
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
        if self.read_only { return reject_read_only(out, outcome, "/compact"); }
        let mut engine = self.engine.write().expect("engine lock poisoned");
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
        if self.read_only { return reject_read_only(out, outcome, "/commit"); }
        let req: CommitRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => {
                outcome.status = 400;
                let detail = format!("commit body: {e}");
                outcome.failure = Some(detail.clone());
                return write_error(out, 400, "bad_json", &detail);
            }
        };
        let mut engine = self.engine.write().expect("engine lock poisoned");
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
        let engine = self.engine.read().expect("engine lock poisoned");
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
        let engine = self.engine.read().expect("engine lock poisoned");
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
        let engine = self.engine.read().expect("engine lock poisoned");
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
        let engine = self.engine.read().expect("engine lock poisoned");
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
        let engine = self.engine.read().expect("engine lock poisoned");
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
        let engine = self.engine.read().expect("engine lock poisoned");
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
        let engine = self.engine.read().expect("engine lock poisoned");
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
        if self.read_only && query_request_has_writes(&req) {
            return reject_read_only(out, outcome, "/query (write clauses)");
        }
        // Writes: exclusive lock + materialised response (the write
        // path needs to mutate the engine and the response shape is
        // small). Reads: shared lock + the streaming JSON projection
        // path (`execute_read_into_buf`) — skips the per-row
        // Vec<JsonValue> + UUID-as-String allocations the materialised
        // path pays for, measured ~7-9% faster end-to-end on
        // executor-routed query shapes like `single_pattern_query`.
        if query_request_has_writes(&req) {
            let mut engine = self.engine.write().expect("engine lock poisoned");
            let resp = match execute_query(&mut engine, req) {
                Ok(r) => r,
                Err(e) => return query_error_to_http(out, outcome, &e),
            };
            outcome.status = 200;
            write_json(out, 200, &resp)
        } else {
            let engine = self.engine.read().expect("engine lock poisoned");
            let mut buf: Vec<u8> = Vec::with_capacity(8192);
            if let Err(e) = ndb_engine::query::execute_read_into_buf(&engine, req, &mut buf) {
                return query_error_to_http(out, outcome, &e);
            }
            outcome.status = 200;
            write_status_line(out, 200)?;
            write!(
                out,
                "Content-Type: application/json; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                buf.len(),
            )?;
            out.write_all(&buf)?;
            Ok(())
        }
    }

    /// `POST /query/text` — like `/query` but the body is the query
    /// SOURCE TEXT (e.g. `match species() as ?s return ?s limit 10`).
    /// The server lexes + parses + resolves names → ids against its
    /// current dictionary snapshot and runs the resulting wire-AST.
    /// Useful for ad-hoc clients (curl, the CLI, the browser SPA) that
    /// don't want to hand-craft AST JSON.
    fn handle_query_text(
        &self,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let text = match std::str::from_utf8(body) {
            Ok(s) => s,
            Err(e) => return bad_json(out, outcome, "query/text body", &e.to_string()),
        };
        // Parse + resolve under a read lock so we can inspect for write
        // clauses BEFORE deciding which execution path (and lock kind)
        // to take. The same read lock also catches read-only mode
        // violations from the surface text without round-tripping
        // through the executor.
        let req = {
            let engine = self.engine.read().expect("engine lock poisoned");
            match ndb_query::parse_resolve(&engine, text) {
                Ok(r) => r,
                Err(e) => {
                    let env = e.envelope();
                    let status = match e {
                        ndb_query::RunError::Parse(_) | ndb_query::RunError::Resolve(_) => 400,
                        ndb_query::RunError::Query(_) | ndb_query::RunError::Engine(_) => 500,
                    };
                    outcome.status = status;
                    outcome.failure = Some(env.detail.clone());
                    return write_json(out, status, &env);
                }
            }
        };
        if self.read_only && query_request_has_writes(&req) {
            return reject_read_only(out, outcome, "/query/text (write clauses)");
        }
        // Read-only requests parallelise on a read lock; writes serialise.
        let resp = if query_request_has_writes(&req) {
            let mut engine = self.engine.write().expect("engine lock poisoned");
            execute_query(&mut engine, req).map_err(ndb_query::RunError::Query)
        } else {
            let engine = self.engine.read().expect("engine lock poisoned");
            ndb_engine::query::execute_read(&engine, req).map_err(ndb_query::RunError::Query)
        };
        match resp {
            Ok(resp) => {
                outcome.status = 200;
                write_json(out, 200, &resp)
            }
            Err(e) => {
                let env = e.envelope();
                let status = match e {
                    ndb_query::RunError::Parse(_) | ndb_query::RunError::Resolve(_) => 400,
                    ndb_query::RunError::Query(_) | ndb_query::RunError::Engine(_)  => 500,
                };
                outcome.status = status;
                outcome.failure = Some(env.detail.clone());
                write_json(out, status, &env)
            }
        }
    }

    /// `POST /query/explain` — like `/query/text` but DOES NOT execute.
    /// Body is the query source text. Server lexes + parses + resolves +
    /// plans and returns the per-atom plan tree (chosen execution order,
    /// cardinality estimate, bound vs newly-bound variables, atom shape).
    ///
    /// Response shape:
    /// ```json
    /// {
    ///   "patterns": <number of patterns in the resolved query>,
    ///   "plan": [{
    ///     "pattern_index": 1,
    ///     "estimated_cardinality": 3,
    ///     "atom_summary": "entity type=100 self=?c filters=1",
    ///     "binds": ["c"],
    ///     "uses": []
    ///   }, ...]
    /// }
    /// ```
    ///
    /// Write clauses (`create` / `delete` / `set` / `merge`) are accepted
    /// in the source text but only the read patterns are planned —
    /// no mutation happens. read-only mode allows this route unconditionally.
    fn handle_query_explain(
        &self,
        body: &[u8],
        out: &mut dyn Write,
        outcome: &mut DispatchOutcome,
    ) -> Result<(), ServerError> {
        let text = match std::str::from_utf8(body) {
            Ok(s) => s,
            Err(e) => return bad_json(out, outcome, "query/explain body", &e.to_string()),
        };
        let engine = self.engine.read().expect("engine lock poisoned");
        let req = match ndb_query::parse_resolve(&engine, text) {
            Ok(r) => r,
            Err(e) => {
                let env = e.envelope();
                let status = match e {
                    ndb_query::RunError::Parse(_) | ndb_query::RunError::Resolve(_) => 400,
                    ndb_query::RunError::Query(_) | ndb_query::RunError::Engine(_) => 500,
                };
                outcome.status = status;
                outcome.failure = Some(env.detail.clone());
                return write_json(out, status, &env);
            }
        };
        let entries = ndb_engine::query::plan::explain(&engine, &req.patterns);
        let resp = serde_json::json!({
            "patterns": req.patterns.len(),
            "plan": entries,
        });
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
        if self.read_only && query_request_has_writes(&req) {
            return reject_read_only(out, outcome, "/query_stream (write clauses)");
        }
        // Read-only requests parallelise on a read lock; writes serialise.
        let resp = if query_request_has_writes(&req) {
            let mut engine = self.engine.write().expect("engine lock poisoned");
            execute_query(&mut engine, req)
        } else {
            let engine = self.engine.read().expect("engine lock poisoned");
            ndb_engine::query::execute_read(&engine, req)
        };
        let resp = match resp {
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
    /// v2.1 closed the connection-acceptor bottleneck: `BoundServer::serve`
    /// and `serve_n` spawn a thread per connection (via
    /// [`std::thread::scope`]), so a `/subscribe` blocked in the condvar
    /// wait no longer queues the `/commit` that would fire the notify.
    /// See `subscribe_wakes_on_concurrent_commit_within_a_millisecond_class_latency`
    /// in the test suite.
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
            let e = self.engine.read().expect("engine lock poisoned");
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
                let e = self.engine.read().expect("engine lock poisoned");
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
            let engine = self.engine.read().expect("engine lock poisoned");
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
    pub fn engine(&self) -> Arc<RwLock<Engine>> {
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

    /// Accept and serve forever. Spawns a fresh thread per connection
    /// — multiple `/subscribe` long-polls + concurrent `/commit`s no
    /// longer queue behind each other, unlocking the condvar-based
    /// notify-on-commit path added in #23.
    ///
    /// Threads borrow back into this `BoundServer` via [`std::thread::scope`],
    /// so the function never returns while connections are in flight.
    /// In practice that's fine — `serve()` runs forever on a healthy
    /// server.
    pub fn serve(&self) -> Result<(), ServerError> {
        std::thread::scope(|scope| {
            for stream in self.listener.incoming() {
                match stream {
                    Ok(s) => {
                        let server = self.server;
                        scope.spawn(move || {
                            if let Err(e) = server.handle_connection(s) {
                                eprintln!("connection error: {e}");
                            }
                        });
                    }
                    Err(e) => eprintln!("accept error: {e}"),
                }
            }
            Ok(())
        })
    }

    /// Accept and serve N connections, then return. Used by tests.
    /// Spawns a thread per accepted connection and joins them all
    /// before returning so the bounded test loop still blocks until
    /// every request has been processed.
    pub fn serve_n(&self, n: usize) -> Result<(), ServerError> {
        std::thread::scope(|scope| -> Result<(), ServerError> {
            let mut handles = Vec::with_capacity(n);
            for _ in 0..n {
                let (stream, _addr) = self.listener.accept()?;
                let server = self.server;
                handles.push(scope.spawn(move || {
                    if let Err(e) = server.handle_connection(stream) {
                        eprintln!("connection error: {e}");
                    }
                }));
            }
            for h in handles {
                // Per-connection errors are already logged; we don't
                // surface them here because the test surface treats
                // serve_n as "drove N requests to completion".
                let _ = h.join();
            }
            Ok(())
        })
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

    /// Accept and serve forever. Spawns a fresh thread per connection
    /// via [`std::thread::scope`]; same shape as the plain-TCP
    /// `BoundServer::serve` — see that for rationale.
    pub fn serve(&self) -> Result<(), ServerError> {
        std::thread::scope(|scope| {
            for stream in self.listener.incoming() {
                match stream {
                    Ok(s) => {
                        let me = self;
                        scope.spawn(move || {
                            if let Err(e) = me.handle_one(s) {
                                eprintln!("tls connection error: {e}");
                            }
                        });
                    }
                    Err(e) => eprintln!("tls accept error: {e}"),
                }
            }
            Ok(())
        })
    }

    /// Accept and serve N connections, then return. Spawns + joins;
    /// see `BoundServer::serve_n`.
    pub fn serve_n(&self, n: usize) -> Result<(), ServerError> {
        std::thread::scope(|scope| -> Result<(), ServerError> {
            let mut handles = Vec::with_capacity(n);
            for _ in 0..n {
                let (stream, _addr) = self.listener.accept()?;
                let me = self;
                handles.push(scope.spawn(move || {
                    if let Err(e) = me.handle_one(stream) {
                        eprintln!("tls connection error: {e}");
                    }
                }));
            }
            for h in handles {
                let _ = h.join();
            }
            Ok(())
        })
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
                | "/query" | "/query/text" | "/query_stream" | "/subscribe",
        ) => Some(Capability::Read),
        _ => None,
    }
}

/// Has at least one mutating clause (create / delete / set / merge)?
fn query_request_has_writes(req: &QueryRequest) -> bool {
    !req.creates.is_empty() || !req.deletes.is_empty()
        || !req.sets.is_empty() || !req.merges.is_empty()
}

/// 403 reply for read-only-mode rejection. Same shape as other
/// `write_error` returns so clients can dispatch by status.
fn reject_read_only(
    out: &mut dyn Write,
    outcome: &mut DispatchOutcome,
    what: &str,
) -> Result<(), ServerError> {
    outcome.status = 403;
    let detail = format!(
        "server is in read-only mode; {what} is not allowed. Writes go through the CLI \
         or a direct /commit on a non-public server."
    );
    outcome.failure = Some(detail.clone());
    write_error(out, 403, "read_only", &detail)
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

// ---------------------------------------------------------------------------
// v2.2 — CORS preflight + streaming ACAO header injector
// ---------------------------------------------------------------------------

fn write_cors_preflight(out: &mut dyn Write, origin: &str) -> Result<(), ServerError> {
    write!(
        out,
        "HTTP/1.1 204 No Content\r\n\
         Access-Control-Allow-Origin: {origin}\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type, Authorization\r\n\
         Access-Control-Max-Age: 600\r\n\
         Connection: close\r\n\r\n",
    )?;
    Ok(())
}

/// Streaming `Write` wrapper that injects a fixed sequence of header
/// bytes (e.g. `Access-Control-Allow-Origin: ...\r\n`) into the first
/// HTTP response header block written through it.
///
/// Buffers the byte stream until `\r\n\r\n` is observed (the
/// header-block terminator), splices the injection in just before the
/// empty CRLF, flushes everything, and then passes subsequent writes
/// through directly. This keeps streaming responses (/iter,
/// /query_stream, /subscribe) streaming after their headers land.
struct HeaderInjector<'a, W: Write> {
    inner: &'a mut W,
    inject: Vec<u8>,
    pending: Vec<u8>,
    headers_done: bool,
}

impl<'a, W: Write> HeaderInjector<'a, W> {
    fn new(inner: &'a mut W, inject: Vec<u8>) -> Self {
        Self {
            inner,
            inject,
            pending: Vec::with_capacity(256),
            headers_done: false,
        }
    }
}

impl<W: Write> Write for HeaderInjector<'_, W> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.headers_done {
            return self.inner.write(b);
        }
        self.pending.extend_from_slice(b);
        if let Some(pos) = self
            .pending
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
        {
            // Skip injection if the response already carries an
            // Access-Control-Allow-Origin header — preflight responses
            // emit their own ACAO inline; double-emitting it produces
            // a `'*, *'` value the browser refuses.
            let header_block = &self.pending[..pos];
            let already_has_acao = header_block
                .windows(b"access-control-allow-origin:".len())
                .any(|w| w.eq_ignore_ascii_case(b"access-control-allow-origin:"));
            if already_has_acao {
                self.inner.write_all(&self.pending)?;
            } else {
                // Insert injection between the last header line's CRLF
                // and the empty CRLF that terminates the header block.
                self.inner.write_all(&self.pending[..pos + 2])?;
                self.inner.write_all(&self.inject)?;
                self.inner.write_all(&self.pending[pos + 2..])?;
            }
            self.pending.clear();
            self.headers_done = true;
        }
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.headers_done && !self.pending.is_empty() {
            // No header terminator was emitted (malformed response —
            // shouldn't happen for our routes). Flush whatever we have
            // so the client sees a non-empty error rather than a hang.
            self.inner.write_all(&self.pending)?;
            self.pending.clear();
        }
        self.inner.flush()
    }
}
