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
           --v1-bind <addr>   Also serve the /v1 data API on <addr> (e.g. 127.0.0.1:8742)\n\
                              from the SAME engine — a write via /mcp is instantly\n\
                              readable via /v1. Requires --http. With NDB_TOKEN, /v1 is\n\
                              fully locked; with only NDB_WRITE_TOKEN, /v1 is read-only\n\
                              (writes go through /mcp); with neither, /v1 is open.\n\
           --tls-cert <path>  PEM cert chain; with --tls-key, --http serves HTTPS\n\
           --tls-key <path>   PEM private key (PKCS#8/PKCS#1/SEC1) — pairs with --tls-cert\n\
           --cors-origin <o>  Emit Access-Control-Allow-Origin: <o> (browser agents).\n\
         \n\
         Environment:\n\
           NDB_MCP_PRINCIPAL  JSON: {{\"name\":..,\"capabilities\":[..]}}; single-principal gate\n\
           NDB_MCP_PRINCIPALS Path to a principals registry (JSON: {{\"principals\":{{\"<token>\":\n\
                              {{\"name\":..,\"capabilities\":[..]}}}}}}). Multi-tenant: each HTTP\n\
                              request's bearer token resolves to a principal gated by its caps,\n\
                              audited under its name (forces audit on).\n\
           NDB_TOKEN          When set, --http requires Authorization: Bearer <token>\n\
           NDB_AUDIT=1        Equivalent to --audit (append <db>/.audit.jsonl per tool call)\n"
    );
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut audit = false;
    let mut http_addr: Option<String> = None;
    let mut v1_addr: Option<String> = None;
    let mut cors_origin: Option<String> = None;
    let mut tls_cert: Option<String> = None;
    let mut tls_key: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--path" | "-p" => path = args.next(),
            "--audit" => audit = true,
            "--http" => http_addr = args.next(),
            "--v1-bind" => v1_addr = args.next(),
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

    // Multi-tenant: a principals registry file (bearer token -> principal with
    // a capability set) enables per-dev tokens + capability scoping over HTTP.
    // Auditing is forced on so every per-dev call is attributable.
    let mut principals_loaded = false;
    if let Ok(path) = std::env::var("NDB_MCP_PRINCIPALS")
        && !path.is_empty()
    {
        match ndb_server::Principals::load(std::path::Path::new(&path)) {
            Ok(Some(p)) => {
                eprintln!(
                    "ndb-mcp-server: multi-tenant auth — {} principals from {path}",
                    p.principals.len()
                );
                server = server.with_principals(p);
                principals_loaded = true;
            }
            Ok(None) => eprintln!("ndb-mcp-server: NDB_MCP_PRINCIPALS file not found: {path}"),
            Err(e) => {
                eprintln!("ndb-mcp-server: invalid NDB_MCP_PRINCIPALS: {e}");
                return ExitCode::from(2);
            }
        }
    }

    let audit_on = audit || principals_loaded || std::env::var("NDB_AUDIT").is_ok_and(|v| v == "1");
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

    // DEPRECATED env knob: `NDB_VECTOR_PROP=<ids>` still works for backward
    // compat, but it's now folded into the first-class persisted path — each id
    // is registered AND written to the `.vector_props` sidecar, so it survives
    // a restart with the env unset. Prefer the `ndb.register_vector_property`
    // MCP tool; this env is a one-shot migration aid.
    if let Ok(spec) = std::env::var("NDB_VECTOR_PROP") {
        for part in spec.split(',') {
            if let Ok(pid) = part.trim().parse::<u32>() {
                match server.register_vector_property_persisted(pid) {
                    Ok(indexed) => eprintln!(
                        "ndb-mcp-server: registered+persisted vector property {pid} \
                         ({indexed} vectors indexed) [NDB_VECTOR_PROP is deprecated; \
                         use the ndb.register_vector_property tool]"
                    ),
                    Err(e) => eprintln!("ndb-mcp-server: vector property {pid} failed: {e}"),
                }
            }
        }
    }

    if v1_addr.is_some() && http_addr.is_none() {
        eprintln!("--v1-bind requires --http (the unified launch serves both surfaces)");
        return ExitCode::from(2);
    }

    // Network transport when --http is given; otherwise the stdio loop.
    if let Some(addr) = http_addr {
        let bearer_token = std::env::var("NDB_TOKEN").ok().filter(|t| !t.is_empty());
        let write_token = std::env::var("NDB_WRITE_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        if bearer_token.is_some() {
            eprintln!("ndb-mcp-server: full bearer-token auth enabled for --http");
        } else if write_token.is_some() {
            eprintln!("ndb-mcp-server: reads open, write tools require NDB_WRITE_TOKEN");
        }

        // Unified launch: serve the /v1 data API from the SAME engine handle on
        // a second thread, so a /mcp write is instantly readable via /v1 (one
        // engine, two surfaces, one process). The v1 server gets a clone of the
        // shared Arc<RwLock<Engine>> — single-writer is preserved by the lock.
        if let Some(v1) = v1_addr {
            let shared = server.engine();
            let mut v1srv = ndb_server::Server::from_shared(shared);
            // Auth parity: full lockdown mirrors /mcp's NDB_TOKEN; with only a
            // write token, /v1 becomes the read surface (writes via /mcp).
            if let Some(tok) = &bearer_token {
                v1srv = v1srv.with_auth_token(tok.clone());
            } else if write_token.is_some() {
                v1srv = v1srv.with_read_only(true);
            }
            if let Some(origin) = &cors_origin {
                v1srv = v1srv.with_cors_origin(origin.clone());
            }
            eprintln!("ndb-mcp-server: also serving /v1 on http://{v1} (shared engine)");
            std::thread::spawn(move || {
                if let Err(e) = v1srv.run(&v1) {
                    eprintln!("ndb-mcp-server: /v1 server error: {e}");
                }
            });
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
