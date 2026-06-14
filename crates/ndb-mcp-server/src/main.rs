//! nDB MCP server binary — stdio JSON-RPC loop.
//!
//! Usage:
//!
//! ```text
//! ndb-mcp-server --path /tmp/mydb [--audit]
//! ```
//!
//! Spawned by an MCP-aware client (Claude Desktop, custom agent, etc.).
//! Reads JSON-RPC requests from stdin, writes responses to stdout, one
//! per line.
//!
//! Optional principal-gating via `NDB_MCP_PRINCIPAL` env:
//!
//! ```text
//! NDB_MCP_PRINCIPAL='{"name":"alice","capabilities":["read","iter"]}'
//! ```
//!
//! When set, every tool call is checked against the principal's
//! capability set. Capabilities map to Capability enum variants (lower
//! snake_case: health, read, iter, commit, flush, compact, admin).
#![allow(clippy::doc_markdown)]

use std::process::ExitCode;

use ndb_mcp_server::{HttpOpts, McpServer, serve_http, serve_https};
use ndb_server::Principal;

fn usage() {
    eprintln!(
        "Usage: ndb-mcp-server --path <database-dir> [--audit] \\\n\
         \t[--http <addr> [--cors-origin <origin>]]\n\
         \n\
         Transports:\n\
           (default)          stdio JSON-RPC loop — for a same-machine MCP client.\n\
           --http <addr>      Streamable HTTP MCP on <addr> (e.g. 127.0.0.1:9000):\n\
                              POST /mcp (JSON-RPC), GET /health. Plain HTTP — front\n\
                              it with a TLS proxy (Pingora), or add --tls-* below.\n\
           --tls-cert <path>  PEM cert chain; with --tls-key, --http serves HTTPS\n\
           --tls-key <path>   PEM private key (PKCS#8/PKCS#1/SEC1) — pairs with --tls-cert\n\
           --cors-origin <o>  Emit Access-Control-Allow-Origin: <o> (browser agents).\n\
         \n\
         Environment:\n\
           NDB_MCP_PRINCIPAL  JSON: {{\"name\":..,\"capabilities\":[..]}}; gates every tool call\n\
           NDB_TOKEN          When set, --http requires Authorization: Bearer <token>\n\
           NDB_AUDIT=1        Equivalent to --audit (append <db>/.audit.jsonl per tool call)\n"
    );
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut audit = false;
    let mut http_addr: Option<String> = None;
    let mut cors_origin: Option<String> = None;
    let mut tls_cert: Option<String> = None;
    let mut tls_key: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--path" | "-p" => path = args.next(),
            "--audit" => audit = true,
            "--http" => http_addr = args.next(),
            "--cors-origin" => cors_origin = args.next(),
            "--tls-cert" => tls_cert = args.next(),
            "--tls-key" => tls_key = args.next(),
            "--help" | "-h" => {
                usage();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown arg: {other}");
                usage();
                return ExitCode::from(2);
            }
        }
    }
    let Some(path) = path else {
        usage();
        return ExitCode::from(2);
    };
    let mut server = match McpServer::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ndb-mcp-server: failed to open {path}: {e}");
            return ExitCode::from(1);
        }
    };

    if let Ok(json) = std::env::var("NDB_MCP_PRINCIPAL")
        && !json.is_empty()
    {
        match serde_json::from_str::<Principal>(&json) {
            Ok(p) => {
                eprintln!(
                    "ndb-mcp-server: principal '{}' with {} capabilities",
                    p.name,
                    p.capabilities.len()
                );
                server = server.with_principal(p);
            }
            Err(e) => {
                eprintln!("ndb-mcp-server: invalid NDB_MCP_PRINCIPAL: {e}");
                return ExitCode::from(2);
            }
        }
    }

    let audit_on = audit || std::env::var("NDB_AUDIT").is_ok_and(|v| v == "1");
    if audit_on {
        server = match server.with_audit_log() {
            Ok(s) => {
                eprintln!("ndb-mcp-server: audit log enabled");
                s
            }
            Err(e) => {
                eprintln!("ndb-mcp-server: audit log failed: {e}");
                return ExitCode::from(1);
            }
        };
    }

    // Network transport when --http is given; otherwise the stdio loop.
    if let Some(addr) = http_addr {
        let bearer_token = std::env::var("NDB_TOKEN").ok().filter(|t| !t.is_empty());
        let write_token = std::env::var("NDB_WRITE_TOKEN").ok().filter(|t| !t.is_empty());
        if bearer_token.is_some() {
            eprintln!("ndb-mcp-server: full bearer-token auth enabled for --http");
        } else if write_token.is_some() {
            eprintln!("ndb-mcp-server: reads open, write tools require NDB_WRITE_TOKEN");
        }
        let opts = HttpOpts {
            bearer_token,
            write_token,
            cors_origin,
        };
        let result = match (tls_cert, tls_key) {
            (Some(cert), Some(key)) => serve_https(&server, &addr, &cert, &key, &opts),
            (None, None) => serve_http(&server, &addr, &opts),
            _ => {
                eprintln!("--tls-cert and --tls-key must be supplied together");
                return ExitCode::from(2);
            }
        };
        if let Err(e) = result {
            eprintln!("ndb-mcp-server: http server error: {e}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    if let Err(e) = server.run_stdio(stdin.lock(), stdout.lock()) {
        eprintln!("ndb-mcp-server: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
