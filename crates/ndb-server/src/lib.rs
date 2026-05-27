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

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::{Arc, Mutex};

use ndb_engine::{
    CommitRequest, CommitResponse, Engine, EngineError, ErrorResponse, JsonRecord, ReadResponse,
    Record, Resolved, TxId, WireError, WriteTxn,
};
use serde::Serialize;
use thiserror::Error;

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
}

impl Server {
    /// Open an existing database (or create one if missing) and prepare
    /// the server for `run` / handle_connection.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, ServerError> {
        let path = path.as_ref();
        let engine = if path.exists() && path.join("CURRENT").exists() {
            Engine::open(path)?
        } else {
            Engine::create(path)?
        };
        Ok(Self {
            engine: Arc::new(Mutex::new(engine)),
        })
    }

    /// Wrap an already-opened engine. Useful for tests.
    #[must_use]
    pub fn from_engine(engine: Engine) -> Self {
        Self {
            engine: Arc::new(Mutex::new(engine)),
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

    /// Handle one connection: parse a single HTTP/1.1 request, dispatch,
    /// write a response, close. (No keep-alive in v1.)
    pub fn handle_connection(&self, mut stream: TcpStream) -> Result<(), ServerError> {
        let (req, body) = parse_request(&mut stream)?;
        self.dispatch(&req, &body, &mut stream)?;
        let _ = stream.flush();
        Ok(())
    }

    fn dispatch(&self, req: &Request, body: &[u8], out: &mut TcpStream) -> Result<(), ServerError> {
        match (req.method.as_str(), req.path_no_query()) {
            ("GET", "/health") => write_json(out, 200, &serde_json::json!({"status": "ok"})),
            ("POST", "/commit") => self.handle_commit(body, out),
            ("GET", path) if path.starts_with("/read/") => {
                let uuid_str = &path["/read/".len()..];
                self.handle_read(uuid_str, out)
            }
            ("GET", "/iter") => self.handle_iter(out),
            ("POST", "/flush") => self.handle_flush(out),
            ("POST", "/compact") => self.handle_compact(out),
            _ => write_error(
                out,
                404,
                "not_found",
                &format!("no route for {} {}", req.method, req.path),
            ),
        }
    }

    fn handle_flush(&self, out: &mut TcpStream) -> Result<(), ServerError> {
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        engine.flush()?;
        let (records, bytes) = engine.memtable_stats();
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

    fn handle_compact(&self, out: &mut TcpStream) -> Result<(), ServerError> {
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let stats = engine.compact()?;
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

    fn handle_commit(&self, body: &[u8], out: &mut TcpStream) -> Result<(), ServerError> {
        let req: CommitRequest = match serde_json::from_slice(body) {
            Ok(r) => r,
            Err(e) => {
                return write_error(out, 400, "bad_json", &format!("commit body: {e}"));
            }
        };
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let mut txn: WriteTxn = engine.begin_write();
        let tx_id = txn.tx_id();
        for jr in req.records {
            let r: Record = match jr.try_into() {
                Ok(r) => r,
                Err(e) => {
                    drop(txn); // rollback
                    return write_error(out, 400, "bad_record", &e.to_string());
                }
            };
            // The server stamps tx_id_assert / tx_id_supersede here so
            // callers don't need to know the next tx. Roles in
            // hyperedges and target_id on tombstones are passed
            // through verbatim.
            stamp_and_push(&mut txn, r);
        }
        match txn.commit() {
            Ok(tid) => write_json(out, 200, &CommitResponse { tx_id: tid.get() }),
            Err(EngineError::Validation(v)) => {
                write_error(out, 422, "validation", &v.to_string())?;
                // Re-bump tx_id allocation already happened; the gap is acceptable.
                let _ = tx_id;
                Ok(())
            }
            Err(e) => write_error(out, 500, "engine", &e.to_string()),
        }
    }

    fn handle_read(&self, uuid_str: &str, out: &mut TcpStream) -> Result<(), ServerError> {
        let Ok(uuid) = uuid::Uuid::parse_str(uuid_str) else {
            return write_error(out, 400, "bad_uuid", uuid_str);
        };
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let snapshot = TxId::new(engine.manifest().last_tx_id);
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
        write_json(out, 200, &body)
    }

    fn handle_iter(&self, out: &mut TcpStream) -> Result<(), ServerError> {
        let mut engine = self.engine.lock().expect("engine mutex poisoned");
        let snapshot = TxId::new(engine.manifest().last_tx_id);
        let records = engine.snapshot_iter(snapshot)?;
        // Write status + headers manually so we can stream JSONL.
        write_status_line(out, 200)?;
        out.write_all(b"Content-Type: application/jsonl; charset=utf-8\r\n")?;
        out.write_all(b"Connection: close\r\n\r\n")?;
        for r in records {
            let jr: JsonRecord = (&r).into();
            let line = serde_json::to_string(&jr).map_err(|e| {
                ServerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })?;
            out.write_all(line.as_bytes())?;
            out.write_all(b"\n")?;
        }
        Ok(())
    }

    /// Borrow the shared engine for direct manipulation (tests).
    #[must_use]
    pub fn engine(&self) -> Arc<Mutex<Engine>> {
        Arc::clone(&self.engine)
    }
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

// ---------------------------------------------------------------------------
// Hand-rolled HTTP/1.1 parsing + response writing
// ---------------------------------------------------------------------------

/// Parsed HTTP request head.
#[derive(Debug)]
struct Request {
    method: String,
    /// Includes the query string, if any.
    path: String,
}

impl Request {
    fn path_no_query(&self) -> &str {
        self.path.split('?').next().unwrap_or(&self.path)
    }
}

fn parse_request(stream: &mut TcpStream) -> Result<(Request, Vec<u8>), ServerError> {
    let mut reader = BufReader::new(stream.try_clone()?);
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
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(ServerError::BadRequest("eof in headers"));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok((Request { method, path }, body))
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

fn write_status_line(out: &mut TcpStream, code: u16) -> std::io::Result<()> {
    write!(out, "HTTP/1.1 {code} {}\r\n", status_text(code))
}

fn write_json<T: Serialize>(out: &mut TcpStream, code: u16, body: &T) -> Result<(), ServerError> {
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

fn write_error(out: &mut TcpStream, code: u16, err: &str, detail: &str) -> Result<(), ServerError> {
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
