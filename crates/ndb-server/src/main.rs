//! nDB server binary. Run with `--path <dir>` and optional `--bind <addr>`.
#![allow(clippy::doc_markdown)]

use std::process::ExitCode;

use ndb_server::Server;

fn usage() {
    eprintln!(
        "Usage: ndb-server --path <database-dir> [--bind 127.0.0.1:8742] [--audit] \\\n\
         \t[--tls-cert <path> --tls-key <path>]\n\
         \n\
         Environment:\n\
           NDB_TOKEN=<token>   Require Authorization: Bearer <token> on every route except /health.\n\
           NDB_AUDIT=1         Equivalent to --audit (append <db>/.audit.jsonl per request).\n\
         \n\
         When both --tls-cert and --tls-key are supplied, the server binds TLS on --bind.\n\
         When only one is supplied or neither, the server binds plain HTTP on --bind.\n\
         \n\
         Routes:\n  GET  /health\n  POST /commit\n  GET  /read/:uuid\n  GET  /iter\n  POST /flush\n  POST /compact\n"
    );
}

struct Args {
    path: String,
    bind: String,
    audit: bool,
    tls_cert: Option<String>,
    tls_key: Option<String>,
}

fn parse_args() -> Option<Args> {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut bind: Option<String> = None;
    let mut audit = false;
    let mut tls_cert: Option<String> = None;
    let mut tls_key: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--path" | "-p" => path = args.next(),
            "--bind" | "-b" => bind = args.next(),
            "--audit" => audit = true,
            "--tls-cert" => tls_cert = args.next(),
            "--tls-key" => tls_key = args.next(),
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
        tls_cert,
        tls_key,
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
    // Decide TLS or plain TCP. Both --tls-cert and --tls-key must be
    // supplied together; either-without-the-other is a config error.
    let use_tls = match (&args.tls_cert, &args.tls_key) {
        (Some(c), Some(k)) => {
            server = match server.with_tls_pem(std::path::Path::new(c), std::path::Path::new(k)) {
                Ok(s) => {
                    eprintln!("ndb-server: TLS enabled (cert={c}, key={k})");
                    s
                }
                Err(e) => {
                    eprintln!("failed to load TLS material: {e}");
                    return ExitCode::from(1);
                }
            };
            true
        }
        (None, None) => false,
        _ => {
            eprintln!("--tls-cert and --tls-key must be supplied together");
            return ExitCode::from(2);
        }
    };
    let run_result = if use_tls {
        server.run_tls(&args.bind)
    } else {
        server.run(&args.bind)
    };
    if let Err(e) = run_result {
        eprintln!("server error: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
