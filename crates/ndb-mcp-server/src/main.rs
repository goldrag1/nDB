//! nDB MCP server binary — stdio JSON-RPC loop.
//!
//! Usage:
//!
//! ```text
//! ndb-mcp-server --path /tmp/mydb
//! ```
//!
//! Spawned by an MCP-aware client (Claude Desktop, custom agent, etc.).
//! Reads JSON-RPC requests from stdin, writes responses to stdout, one
//! per line.
#![allow(clippy::doc_markdown)]

use std::process::ExitCode;

use ndb_mcp_server::McpServer;

fn usage() {
    eprintln!("Usage: ndb-mcp-server --path <database-dir>");
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--path" | "-p" => path = args.next(),
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
    let server = match McpServer::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ndb-mcp-server: failed to open {path}: {e}");
            return ExitCode::from(1);
        }
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    if let Err(e) = server.run_stdio(stdin.lock(), stdout.lock()) {
        eprintln!("ndb-mcp-server: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
