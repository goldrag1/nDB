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

use ndb_mcp_server::McpServer;
use ndb_server::Principal;

fn usage() {
    eprintln!(
        "Usage: ndb-mcp-server --path <database-dir> [--audit]\n\
         \n\
         Environment:\n\
           NDB_MCP_PRINCIPAL  JSON: {{\"name\":..,\"capabilities\":[..]}}; gates every tool call\n\
           NDB_AUDIT=1        Equivalent to --audit (append <db>/.audit.jsonl per tool call)\n"
    );
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut audit = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--path" | "-p" => path = args.next(),
            "--audit" => audit = true,
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

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    if let Err(e) = server.run_stdio(stdin.lock(), stdout.lock()) {
        eprintln!("ndb-mcp-server: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
