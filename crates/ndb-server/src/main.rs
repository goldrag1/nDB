//! nDB server binary. Run with `--path <dir>` and optional `--bind <addr>`.
#![allow(clippy::doc_markdown)]

use std::process::ExitCode;

use ndb_server::Server;

fn usage() {
    eprintln!(
        "Usage: ndb-server --path <database-dir> [--bind 127.0.0.1:8742]\n\
         \n\
         Routes:\n  GET  /health\n  POST /commit\n  GET  /read/:uuid\n  GET  /iter\n  POST /flush\n  POST /compact\n"
    );
}

fn parse_args() -> Option<(String, String)> {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut bind: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--path" | "-p" => path = args.next(),
            "--bind" | "-b" => bind = args.next(),
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
    Some((path?, bind.unwrap_or_else(|| "127.0.0.1:8742".to_owned())))
}

fn main() -> ExitCode {
    let Some((path, bind)) = parse_args() else {
        return ExitCode::from(2);
    };
    let mut server = match Server::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to open database at {path}: {e}");
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
    if let Err(e) = server.run(&bind) {
        eprintln!("server error: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
