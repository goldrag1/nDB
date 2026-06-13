//! nDB sharding router binary.
//!
//!   ndb-router --bind 0.0.0.0:8740 --shards http://shard-0:8742,http://shard-1:8742
//!
//! Routes `/v1` requests across the listed shards (read path: health, point
//! reads, scatter-gather iter). Point clients/agents at the router exactly as
//! they would a single nDB server.
#![allow(missing_docs)]

use std::process::ExitCode;
use std::sync::Arc;

use ndb_router::{Router, ShardMap};

fn usage() {
    eprintln!(
        "Usage: ndb-router --shards <url[,url...]> [--bind 127.0.0.1:8740]\n\
         \n\
         --shards   Comma-separated shard base URLs (e.g. http://s0:8742,http://s1:8742).\n\
         --bind     Address to listen on (default 127.0.0.1:8740).\n\
         \n\
         Routes /v1: GET /health, GET /read/:id (point-routed by hash(id)%N),\n\
         GET /iter (scatter-gather). Other routes return 501 in this increment."
    );
}

fn main() -> ExitCode {
    let mut bind = "127.0.0.1:8740".to_string();
    let mut shards: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" | "-b" => {
                if let Some(v) = args.next() {
                    bind = v;
                }
            }
            "--shards" | "-s" => {
                if let Some(v) = args.next() {
                    shards = v
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
            "--help" | "-h" => {
                usage();
                return ExitCode::from(2);
            }
            other => {
                eprintln!("unknown arg: {other}");
                usage();
                return ExitCode::from(2);
            }
        }
    }
    if shards.is_empty() {
        eprintln!("--shards is required (at least one shard URL)");
        usage();
        return ExitCode::from(2);
    }
    eprintln!("ndb-router: {} shard(s), listening on {bind}", shards.len());
    for (i, s) in shards.iter().enumerate() {
        eprintln!("  shard {i}: {s}");
    }
    let router = Arc::new(Router::new(ShardMap::new(shards)));
    if let Err(e) = router.serve(&bind) {
        eprintln!("router error: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
