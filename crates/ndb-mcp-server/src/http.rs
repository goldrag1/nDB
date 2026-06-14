//! Streamable HTTP transport for the MCP server.
//!
//! The stdio transport ([`crate::McpServer::run_stdio`]) only serves an agent
//! running on the same machine. This module adds the network transport so a
//! *remote* agent can reach the same tools over HTTP, behind a reverse proxy
//! (Pingora / cloudflared) that terminates TLS.
//!
//! Shape: **Streamable HTTP** (MCP 2025-03-26), the successor to the deprecated
//! two-endpoint HTTP+SSE transport. A single endpoint:
//!
//! - `POST /mcp` — body is one JSON-RPC request; the response is the JSON-RPC
//!   result as `application/json`. (v1 always replies with JSON; the optional
//!   SSE upgrade for server-streamed messages is a v2 conversation.)
//! - `GET  /mcp` — `405`: this server offers no server→client SSE stream, which
//!   the spec explicitly permits.
//! - `GET  /health` — `200 {"status":"ok"}`, unauthenticated (proxy liveness).
//! - `OPTIONS *` — CORS preflight when `--cors-origin` is set.
//!
//! Dispatch is shared with stdio: every request runs through
//! [`crate::McpServer::handle_line`], so the tool surface, principal gating, and
//! audit log are identical across transports. Single-threaded accept loop,
//! matching ndb-server's v1 design — real concurrency lives in the proxy in
//! front (connection pooling) and is a v2 conversation here.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use crate::McpServer;

/// Hard cap on a POST body. Exceeding it yields `413` and the body is never
/// read past the cap.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Configuration for the Streamable HTTP MCP transport.
#[derive(Debug, Default, Clone)]
pub struct HttpOpts {
    /// When set, EVERY `POST /mcp` must carry `Authorization: Bearer <token>`
    /// (full lockdown). `/health` is always exempt for proxy liveness.
    pub bearer_token: Option<String>,
    /// When set (and `bearer_token` is not), reads are open but *write* tools
    /// (`ndb.commit_entity` / `ndb.commit_hyperedge`) require this bearer token.
    /// Lets a public read-only UI and a token-bearing writer share one endpoint.
    pub write_token: Option<String>,
    /// When set, emit `Access-Control-Allow-Origin: <value>` on every response
    /// and answer `OPTIONS` preflight — so a browser agent on another origin
    /// can call the endpoint.
    pub cors_origin: Option<String>,
}

/// MCP tool names that mutate the database — gated by `write_token`.
const WRITE_TOOLS: &[&str] = &["ndb.commit_entity", "ndb.commit_hyperedge"];

/// True when the JSON-RPC body is a `tools/call` for a mutating tool.
fn is_write_body(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    if v.get("method").and_then(serde_json::Value::as_str) != Some("tools/call") {
        return false;
    }
    let name = v
        .get("params")
        .and_then(|p| p.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    WRITE_TOOLS.contains(&name)
}

fn bearer_ok(authorization: Option<&str>, token: &str) -> bool {
    authorization
        .and_then(|a| a.strip_prefix("Bearer "))
        .is_some_and(|t| t == token)
}

/// Bind `addr` and serve the Streamable HTTP MCP endpoint until the process is
/// killed. One connection is handled at a time (the engine is single-writer;
/// concurrency belongs in the proxy in front).
pub fn serve_http(server: &McpServer, addr: &str, opts: &HttpOpts) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    eprintln!(
        "ndb-mcp-server: Streamable HTTP MCP on http://{addr}/mcp (POST JSON-RPC); GET /health{}",
        if opts.bearer_token.is_some() {
            "  [bearer-token auth enabled]"
        } else {
            ""
        }
    );
    for conn in listener.incoming() {
        match conn {
            Ok(mut stream) => {
                if let Err(e) = handle_connection(server, &mut stream, opts) {
                    eprintln!("ndb-mcp-server: connection error: {e}");
                }
            }
            Err(e) => eprintln!("ndb-mcp-server: accept error: {e}"),
        }
    }
    Ok(())
}

/// Parse one HTTP/1.1 request off `stream`, route it, and write the response.
/// Generic over any `Read + Write` so it serves a plain `TcpStream` *or* a
/// `rustls::StreamOwned` TLS stream through one code path. `pub(crate)` so the
/// transport tests can drive it without a full accept loop.
pub(crate) fn handle_connection<S: Read + Write>(
    server: &McpServer,
    stream: &mut S,
    opts: &HttpOpts,
) -> std::io::Result<()> {
    let method;
    let path;
    let mut content_length: usize = 0;
    let mut authorization: Option<String> = None;
    let mut body: Vec<u8> = Vec::new();

    // Read the whole request in a borrow scope, then write on the same stream.
    // (A single TLS stream can't be `try_clone`d, so we never split the halves.)
    {
        let mut reader = BufReader::new(&mut *stream);

        // ---- request line ----
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(()); // client closed before sending anything
        }
        let mut parts = request_line.split_whitespace();
        method = parts.next().unwrap_or("").to_owned();
        path = parts.next().unwrap_or("").to_owned();

        // ---- headers ----
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break; // blank line ends the header block
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                match k.trim().to_ascii_lowercase().as_str() {
                    "content-length" => content_length = v.trim().parse().unwrap_or(0),
                    "authorization" => authorization = Some(v.trim().to_owned()),
                    _ => {}
                }
            }
        }

        // ---- body ---- (within the cap; oversize is rejected after the scope)
        if content_length > 0 && content_length <= MAX_BODY_BYTES {
            body = vec![0_u8; content_length];
            reader.read_exact(&mut body)?;
        }
    } // reader borrow released — `stream` is writable again

    // ---- routing ----
    if method == "OPTIONS" {
        return write_response(stream, 204, "No Content", "", "", opts);
    }
    if method == "GET" && path == "/health" {
        return write_json(stream, 200, "OK", r#"{"status":"ok"}"#, opts);
    }
    if path.starts_with("/mcp") {
        if method == "GET" {
            // Spec: a server that offers no SSE stream MUST return 405 here.
            return write_json(
                stream,
                405,
                "Method Not Allowed",
                r#"{"error":"no SSE stream; POST a JSON-RPC request to /mcp"}"#,
                opts,
            );
        }
        if method == "POST" {
            let auth = authorization.as_deref();
            if let Some(token) = &opts.bearer_token {
                // full lockdown: every request needs the token
                if !bearer_ok(auth, token) {
                    return write_json(stream, 401, "Unauthorized",
                        r#"{"error":"missing or invalid bearer token"}"#, opts);
                }
            } else if let Some(wt) = &opts.write_token {
                // reads open; only write tools need the token
                if is_write_body(&body) && !bearer_ok(auth, wt) {
                    return write_json(stream, 401, "Unauthorized",
                        r#"{"error":"write tools require Authorization: Bearer <token>"}"#, opts);
                }
            }
            if content_length > MAX_BODY_BYTES {
                return write_json(
                    stream,
                    413,
                    "Payload Too Large",
                    r#"{"error":"request body too large"}"#,
                    opts,
                );
            }
            let body_str = String::from_utf8_lossy(&body);
            let resp = server.handle_line(body_str.as_ref());
            let json = serde_json::to_string(&resp).unwrap_or_else(|_| {
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"serialize failed"}}"#
                    .to_owned()
            });
            return write_json(stream, 200, "OK", &json, opts);
        }
    }

    write_json(stream, 404, "Not Found", r#"{"error":"not found"}"#, opts)
}

/// Write a JSON response (sets `Content-Type: application/json`).
fn write_json<W: Write>(
    stream: &mut W,
    code: u16,
    reason: &str,
    body: &str,
    opts: &HttpOpts,
) -> std::io::Result<()> {
    write_response(stream, code, reason, "application/json", body, opts)
}

/// Write a complete HTTP/1.1 response with `Connection: close`. An empty
/// `content_type` omits the header (used for the 204 preflight).
fn write_response<W: Write>(
    stream: &mut W,
    code: u16,
    reason: &str,
    content_type: &str,
    body: &str,
    opts: &HttpOpts,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {code} {reason}\r\n");
    if !content_type.is_empty() {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str("Connection: close\r\n");
    if let Some(origin) = &opts.cors_origin {
        head.push_str(&format!("Access-Control-Allow-Origin: {origin}\r\n"));
        head.push_str("Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n");
        head.push_str("Access-Control-Allow-Headers: content-type, authorization\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// TLS termination (rustls) — mirrors ndb-server's --tls-cert/--tls-key so the
// MCP server can serve HTTPS standalone, no proxy required.
// ---------------------------------------------------------------------------

/// Build a rustls `ServerConfig` from a PEM cert chain + private key (ring
/// provider, no client auth, single cert) — same loader shape as ndb-server.
fn build_rustls_config(cert_path: &str, key_path: &str) -> std::io::Result<rustls::ServerConfig> {
    use rustls_pemfile::Item;
    let cert_bytes = std::fs::read(cert_path)?;
    let mut cert_reader = std::io::BufReader::new(cert_bytes.as_slice());
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no PEM certificates found",
        ));
    }
    let key_bytes = std::fs::read(key_path)?;
    let mut key_reader = std::io::BufReader::new(key_bytes.as_slice());
    let key = loop {
        match rustls_pemfile::read_one(&mut key_reader)? {
            Some(Item::Pkcs8Key(k)) => break rustls::pki_types::PrivateKeyDer::Pkcs8(k),
            Some(Item::Pkcs1Key(k)) => break rustls::pki_types::PrivateKeyDer::Pkcs1(k),
            Some(Item::Sec1Key(k)) => break rustls::pki_types::PrivateKeyDer::Sec1(k),
            Some(_) => {}
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "no private key found",
                ));
            }
        }
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| std::io::Error::other(format!("rustls protocol error: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| std::io::Error::other(format!("rustls server cert error: {e}")))
}

/// Like [`serve_http`] but terminates TLS itself (rustls). Lets the MCP server
/// serve HTTPS directly — no proxy required — using the same PEM cert/key shape
/// as ndb-server's `--tls-cert/--tls-key`.
pub fn serve_https(
    server: &McpServer,
    addr: &str,
    cert_path: &str,
    key_path: &str,
    opts: &HttpOpts,
) -> std::io::Result<()> {
    let cfg = Arc::new(build_rustls_config(cert_path, key_path)?);
    let listener = TcpListener::bind(addr)?;
    eprintln!(
        "ndb-mcp-server: Streamable HTTPS MCP on https://{addr}/mcp (TLS, POST JSON-RPC); GET /health{}",
        if opts.bearer_token.is_some() {
            "  [bearer-token auth enabled]"
        } else {
            ""
        }
    );
    for conn in listener.incoming() {
        match conn {
            Ok(tcp) => {
                if let Err(e) = handle_tls_connection(server, tcp, &cfg, opts) {
                    eprintln!("ndb-mcp-server: TLS connection error: {e}");
                }
            }
            Err(e) => eprintln!("ndb-mcp-server: accept error: {e}"),
        }
    }
    Ok(())
}

/// Wrap an accepted `TcpStream` in a rustls `ServerConnection` and dispatch it
/// through the shared [`handle_connection`] path.
fn handle_tls_connection(
    server: &McpServer,
    tcp: TcpStream,
    cfg: &Arc<rustls::ServerConfig>,
    opts: &HttpOpts,
) -> std::io::Result<()> {
    let conn = rustls::ServerConnection::new(Arc::clone(cfg))
        .map_err(|e| std::io::Error::other(format!("rustls: {e}")))?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    handle_connection(server, &mut tls, opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ndb-mcp-http-{}-{}", name, uuid::Uuid::now_v7().simple()));
        p
    }

    /// Drive one real TCP request through `handle_connection` and return the
    /// raw HTTP response. Server stays on this thread (no `Send` needed); a
    /// client thread does the socket I/O so neither side blocks the other.
    fn round_trip(server: &McpServer, opts: &HttpOpts, request: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(request.as_bytes()).unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).unwrap(); // completes when server closes
            resp
        });
        let (mut stream, _) = listener.accept().unwrap();
        handle_connection(server, &mut stream, opts).unwrap();
        drop(stream); // Connection: close → unblock the client's read_to_string
        client.join().unwrap()
    }

    #[test]
    fn post_mcp_initialize_returns_json_result() {
        let dir = temp_dir("init");
        let server = McpServer::open(&dir).unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let request = "POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 46\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}";
        assert_eq!(body.len(), 46, "fixture content-length must match body");
        let resp = round_trip(&server, &HttpOpts::default(), request);
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
        assert!(resp.contains("application/json"), "got: {resp}");
        assert!(resp.contains("ndb-mcp-server"), "missing serverInfo: {resp}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn health_is_unauthenticated() {
        let dir = temp_dir("health");
        let server = McpServer::open(&dir).unwrap();
        let opts = HttpOpts {
            bearer_token: Some("secret".to_owned()),
            write_token: None,
            cors_origin: None,
        };
        let request = "GET /health HTTP/1.1\r\nHost: x\r\n\r\n";
        let resp = round_trip(&server, &opts, request);
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
        assert!(resp.contains("\"status\":\"ok\""), "got: {resp}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn post_mcp_without_token_is_rejected() {
        let dir = temp_dir("auth");
        let server = McpServer::open(&dir).unwrap();
        let opts = HttpOpts {
            bearer_token: Some("secret".to_owned()),
            write_token: None,
            cors_origin: None,
        };
        // tools/list with no Authorization header.
        let request = "POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Length: 45\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}";
        let resp = round_trip(&server, &opts, request);
        assert!(resp.starts_with("HTTP/1.1 401"), "got: {resp}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn get_mcp_returns_405() {
        let dir = temp_dir("get405");
        let server = McpServer::open(&dir).unwrap();
        let request = "GET /mcp HTTP/1.1\r\nHost: x\r\n\r\n";
        let resp = round_trip(&server, &HttpOpts::default(), request);
        assert!(resp.starts_with("HTTP/1.1 405"), "got: {resp}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
