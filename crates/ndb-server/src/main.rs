//! nDB server binary. Run with `--path <dir>` and optional `--bind <addr>`.
#![allow(clippy::doc_markdown)]

use std::process::ExitCode;

use ndb_server::Server;

fn usage() {
    eprintln!(
        "Usage: ndb-server --path <database-dir> [--bind 127.0.0.1:8742] [--audit]\n\
         \n\
         Environment:\n\
           NDB_TOKEN=<token>   Require Authorization: Bearer <token> on every route except /health.\n\
           NDB_AUDIT=1         Equivalent to --audit (append <db>/.audit.jsonl per request).\n\
         \n\
         Routes:\n  GET  /health\n  POST /commit\n  GET  /read/:uuid\n  GET  /iter\n  POST /flush\n  POST /compact\n"
    );
}

struct Args {
    path: String,
    bind: String,
    audit: bool,
}

fn parse_args() -> Option<Args> {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut bind: Option<String> = None;
    let mut audit = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--path" | "-p" => path = args.next(),
            "--bind" | "-b" => bind = args.next(),
            "--audit" => audit = true,
            "--help" | "-h" => {
                usage();
                return None;
            }
            other => {
                eprintln!("unknown arg: {other}");
                usage();
                return None;
            }
        }
    }
    Some(Args {
        path: path?,
        bind: bind.unwrap_or_else(|| "127.0.0.1:8742".to_owned()),
        audit,
    })
}

fn main() -> ExitCode {
    let Some(args) = parse_args() else {
        return ExitCode::from(2);
    };
    let mut server = match Server::open(&args.path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to open database at {}: {e}", args.path);
            return ExitCode::from(1);
        }
    };
    if let Ok(token) = std::env::var("NDB_TOKEN")
        && !token.is_empty()
    {
        eprintln!(
            "ndb-server: bearer-token auth enabled (token len {})",
            token.len()
        );
        server = server.with_auth_token(token);
    }
    // Principals registry from <db>/.principals.json, if present.
    match server.with_principals_from_db() {
        Ok((s, true)) => {
            eprintln!("ndb-server: principals registry loaded from .principals.json");
            server = s;
        }
        Ok((s, false)) => server = s,
        Err(e) => {
            eprintln!("failed to load .principals.json: {e}");
            return ExitCode::from(1);
        }
    }
    let audit_on =
        args.audit || std::env::var("NDB_AUDIT").is_ok_and(|v| v == "1");
    if audit_on {
        server = match server.with_audit_log() {
            Ok(s) => {
                eprintln!("ndb-server: audit log enabled");
                s
            }
            Err(e) => {
                eprintln!("failed to open audit log: {e}");
                return ExitCode::from(1);
            }
        };
    }
    if let Err(e) = server.run(&args.bind) {
        eprintln!("server error: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
