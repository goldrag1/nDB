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

use crate::McpServer;

/// Hard cap on a POST body. Exceeding it yields `413` and the body is never
/// read past the cap.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Configuration for the Streamable HTTP MCP transport.
#[derive(Debug, Default, Clone)]
pub struct HttpOpts {
    /// When set, every `POST /mcp` must carry `Authorization: Bearer <token>`.
    /// `/health` is always exempt so a proxy can probe liveness.
    pub bearer_token: Option<String>,
    /// When set, emit `Access-Control-Allow-Origin: <value>` on every response
    /// and answer `OPTIONS` preflight — so a browser agent on another origin
    /// can call the endpoint.
    pub cors_origin: Option<String>,
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
/// `pub(crate)` so the transport tests can drive it without a full accept loop.
pub(crate) fn handle_connection(
    server: &McpServer,
    stream: &mut TcpStream,
    opts: &HttpOpts,
) -> std::io::Result<()> {
    // Read with a clone so the write half stays usable for the response.
    let mut reader = BufReader::new(stream.try_clone()?);

    // ---- request line ----
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(()); // client closed before sending anything
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();

    // ---- headers ----
    let mut content_length: usize = 0;
    let mut authorization: Option<String> = None;
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
            if let Some(token) = &opts.bearer_token {
                let ok = authorization
                    .as_deref()
                    .and_then(|a| a.strip_prefix("Bearer "))
                    .is_some_and(|t| t == token);
                if !ok {
                    return write_json(
                        stream,
                        401,
                        "Unauthorized",
                        r#"{"error":"missing or invalid bearer token"}"#,
                        opts,
                    );
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
            let mut body = vec![0_u8; content_length];
            reader.read_exact(&mut body)?;
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
fn write_json(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    body: &str,
    opts: &HttpOpts,
) -> std::io::Result<()> {
    write_response(stream, code, reason, "application/json", body, opts)
}

/// Write a complete HTTP/1.1 response with `Connection: close`. An empty
/// `content_type` omits the header (used for the 204 preflight).
fn write_response(
    stream: &mut TcpStream,
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
