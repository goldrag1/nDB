//! nDB server binary. Run with `--path <dir>` and optional `--bind <addr>`.
#![allow(clippy::doc_markdown, dead_code, missing_docs)] // bench-mode constants are pub for clients; docs live in the comment block above each group.

use std::process::ExitCode;

use ndb_engine::{PropertyId, TypeId};
use ndb_server::Server;

// =====================================================================
// Bench-mode schema constants — exported so benchmark clients can compile
// against the same shape. Pre-registered indexes on `--bench-mode`.
// =====================================================================

// ---- "simple" workload (user entities + friends hyperedges) ----
/// Type id used for bench-mode user entities.
pub const BENCH_TYPE_USER: u32 = 1;
/// Property id for `name` (Utf8).
pub const BENCH_PROP_NAME: u32 = 10;
/// Property id for `email` — registered as a lookup-key in `--bench-mode`.
pub const BENCH_PROP_EMAIL: u32 = 11;
/// Property id for `age` — registered for property-B-tree in `--bench-mode`.
pub const BENCH_PROP_AGE: u32 = 12;
/// Property id for `vector` — registered for vector search in `--bench-mode`.
pub const BENCH_PROP_VECTOR: u32 = 13;

// ---- "biology" workload (drug / protein / disease / publication) ----
pub const BIO_TYPE_DRUG: u32 = 100;
pub const BIO_TYPE_PROTEIN: u32 = 101;
pub const BIO_TYPE_DISEASE: u32 = 102;
pub const BIO_TYPE_PUBLICATION: u32 = 103;
// Properties (30..=41).
pub const BIO_PROP_BIO_NAME: u32 = 30;
pub const BIO_PROP_DRUG_CLASS: u32 = 31;
pub const BIO_PROP_GENE_SYMBOL: u32 = 32; // lookup-indexed
pub const BIO_PROP_ORGANISM: u32 = 33;
pub const BIO_PROP_MESH_ID: u32 = 34;
pub const BIO_PROP_PREVALENCE: u32 = 35; // btree-indexed per Disease
pub const BIO_PROP_PUBMED_ID: u32 = 36;
pub const BIO_PROP_JOURNAL: u32 = 37;
pub const BIO_PROP_YEAR: u32 = 38; // btree-indexed per Publication
pub const BIO_PROP_SMILES_EMB: u32 = 39; // vector-indexed
pub const BIO_PROP_SEQUENCE_EMB: u32 = 40; // vector-indexed
pub const BIO_PROP_ABSTRACT_EMB: u32 = 41; // vector-indexed
// Hyperedge types (3-ary and 4-ary).
pub const BIO_TYPE_TARGETS: u32 = 200; // drug + protein + effect
pub const BIO_TYPE_IMPLICATED_IN: u32 = 201; // protein + disease + pathway
pub const BIO_TYPE_CITED_BY: u32 = 202; // drug + disease + publication + evidence_level

fn usage() {
    eprintln!(
        "Usage: ndb-server --path <database-dir> [--bind 127.0.0.1:8742] [--audit] \\\n\
         \t[--bench-mode] [--tls-cert <path> --tls-key <path>]\n\
         \n\
         --bench-mode pre-registers a known schema (type=1, props 10..13) so the\n\
         indexed routes return real hits against a fresh database. Used by the\n\
         Rust-vs-Python benchmark dashboard. Does not change any other behaviour.\n\
         \n\
         Environment:\n\
           NDB_TOKEN=<token>         Require Authorization: Bearer <token> on every route except /health, /ready, /metrics.\n\
           NDB_AUDIT=1               Equivalent to --audit (append <db>/.audit.jsonl per request).\n\
           NDB_MAX_CONNECTIONS=<n>   Cap simultaneously-handled connections (default 256). Excess gets 503.\n\
         \n\
         When both --tls-cert and --tls-key are supplied, the server binds TLS on --bind.\n\
         When only one is supplied or neither, the server binds plain HTTP on --bind.\n\
         \n\
         Routes:\n\
           GET  /health           — liveness; always 200\n\
           GET  /ready            — readiness; 200 when engine usable, 503 while draining\n\
           GET  /metrics          — Prometheus text exposition\n\
           POST /admin/shutdown   — request graceful shutdown (Admin capability)\n\
           POST /commit\n\
           GET  /read/:uuid\n\
           GET  /iter\n\
           POST /flush\n\
           POST /compact\n\
           POST /lookup           — find by external lookup-key\n\
           POST /vector_search    — k-NN over a vector property\n\
           POST /property_lookup  — exact match on (type, property, value)\n\
           POST /property_range   — range query on (type, property)\n"
    );
}

struct Args {
    path: String,
    bind: String,
    audit: bool,
    bench_mode: bool,
    tls_cert: Option<String>,
    tls_key: Option<String>,
}

fn parse_args() -> Option<Args> {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut bind: Option<String> = None;
    let mut audit = false;
    let mut bench_mode = false;
    let mut tls_cert: Option<String> = None;
    let mut tls_key: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--path" | "-p" => path = args.next(),
            "--bind" | "-b" => bind = args.next(),
            "--audit" => audit = true,
            "--bench-mode" => bench_mode = true,
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
        bench_mode,
        tls_cert,
        tls_key,
    })
}

/// Apply optional resource-limit overrides sourced from the environment.
/// Currently `NDB_MAX_CONNECTIONS`; other knobs keep their library defaults
/// and are tuned in code via the `Server::with_*` builders.
fn apply_env_resource_limits(mut server: Server) -> Server {
    if let Ok(v) = std::env::var("NDB_MAX_CONNECTIONS")
        && let Ok(n) = v.parse::<usize>()
        && n > 0
    {
        eprintln!("ndb-server: max_connections = {n}");
        server = server.with_max_connections(n);
    }
    server
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
    if args.bench_mode {
        let engine = server.engine();
        let mut e = engine.write().expect("engine lock poisoned");
        // Simple workload schema.
        e.register_lookup_key(PropertyId::new(BENCH_PROP_EMAIL));
        e.register_property_btree(
            TypeId::new(BENCH_TYPE_USER),
            PropertyId::new(BENCH_PROP_AGE),
        );
        e.register_vector_property(PropertyId::new(BENCH_PROP_VECTOR));
        // Biology workload schema.
        e.register_lookup_key(PropertyId::new(BIO_PROP_GENE_SYMBOL));
        e.register_property_btree(
            TypeId::new(BIO_TYPE_PUBLICATION),
            PropertyId::new(BIO_PROP_YEAR),
        );
        e.register_property_btree(
            TypeId::new(BIO_TYPE_DISEASE),
            PropertyId::new(BIO_PROP_PREVALENCE),
        );
        e.register_vector_property(PropertyId::new(BIO_PROP_SMILES_EMB));
        e.register_vector_property(PropertyId::new(BIO_PROP_SEQUENCE_EMB));
        e.register_vector_property(PropertyId::new(BIO_PROP_ABSTRACT_EMB));
        eprintln!(
            "ndb-server: --bench-mode active — registered indexes for simple and biology workloads",
        );
    }
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
    server = apply_env_resource_limits(server);
    let audit_on = args.audit || std::env::var("NDB_AUDIT").is_ok_and(|v| v == "1");
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
