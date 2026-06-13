//! nDB Studio launcher: open or create an nDB, serve the local UI + API, and
//! open a browser at it. The engine is linked in-process — one binary is the
//! whole application.
//!
//! Usage:
//!   ndb-studio <db-path>            open an existing database
//!   ndb-studio --new <db-path>      create a new database
//!   ndb-studio <db-path> [flags]
//!
//! Flags:
//!   --bind <addr>     listen address (default 127.0.0.1:0 — an ephemeral port)
//!   --low-memory      open in mmap/low-RAM mode (bounded committed memory)
//!   --no-open         do not launch a browser
//!   --public-read     serve read routes to unauthenticated callers (edits/admin still need login)
//!   --merge <out> <src>...   fuse several databases into one (UUID-preserving), then exit

use std::process::ExitCode;
use std::sync::Arc;

use ndb_engine::{Engine, EngineConfig, SharedEngine};
use ndb_studio::store::Store;
use ndb_studio::{http, identity};

const DEFAULT_CACHE_BYTES: usize = 256 * 1024 * 1024;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positionals: Vec<String> = Vec::new();
    let mut bind = "127.0.0.1:0".to_string();
    let mut new = false;
    let mut low_memory = false;
    let mut open = true;
    let mut public_read = false;
    let mut merge = false;
    let mut seed = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--new" => new = true,
            "--low-memory" => low_memory = true,
            "--no-open" => open = false,
            "--public-read" => public_read = true,
            "--merge" => merge = true,
            "--seed-demo" => seed = true,
            "--bind" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("--bind needs an address");
                    return ExitCode::FAILURE;
                };
                bind.clone_from(v);
            }
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("unknown flag: {other}");
                return ExitCode::FAILURE;
            }
            other => positionals.push(other.to_string()),
        }
        i += 1;
    }

    if merge {
        return run_merge(&positionals, low_memory);
    }

    if seed {
        return run_seed(&positionals, low_memory);
    }

    let Some(path) = positionals.into_iter().next() else {
        print_usage();
        return ExitCode::FAILURE;
    };

    let engine = match open_engine(&path, new, low_memory) {
        Ok(e) => e,
        Err(msg) => {
            eprintln!("ndb-studio: {msg}");
            return ExitCode::FAILURE;
        }
    };

    let store = Store::new(engine);
    bootstrap_admin(&store);
    let p = std::path::Path::new(&path);
    let primary_name = p
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("primary")
        .to_string();
    // New databases live in a sibling "<name>-dbs" directory.
    let root = p
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(format!("{primary_name}-dbs"));
    let state = Arc::new(
        http::AppState::new(Arc::new(store), primary_name, root).with_public_read(public_read),
    );
    let listener = match http::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ndb-studio: cannot bind {bind}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
    let url = format!("http://127.0.0.1:{port}/");

    println!("nDB Studio — {path}");
    println!("  open: {url}");
    if open {
        open_browser(&url);
    }

    if let Err(e) = http::run(&listener, &state) {
        eprintln!("ndb-studio: server error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// On a database with no accounts yet, create an `admin` user with a random
/// password printed once to the console, so a fresh (or pre-auth) database is
/// immediately usable. Once any user exists this is a no-op.
fn bootstrap_admin(store: &Store) {
    if store.has_any_user() {
        return;
    }
    let password = identity::random_password();
    let hash = identity::hash_password(&password);
    match store.create_user("admin", &hash, "admin") {
        Ok(_) => {
            println!("  bootstrap admin — username: admin  password: {password}");
            println!("  (shown once; add your own accounts in the Users panel)");
        }
        Err(e) => eprintln!(
            "  warning: could not create bootstrap admin: {}",
            e.message()
        ),
    }
}

/// `--seed-demo <path>` — create a fresh database and populate it with the
/// named, hyperedge- and vector-rich demo dataset (proteins · exoplanets ·
/// species). One-shot; prints the result and exits.
fn run_seed(positionals: &[String], low_memory: bool) -> ExitCode {
    let Some(path) = positionals.first() else {
        eprintln!("--seed-demo needs a <path> for the new database");
        return ExitCode::FAILURE;
    };
    let engine = match open_engine(path, true, low_memory) {
        Ok(e) => e,
        Err(msg) => {
            eprintln!("ndb-studio --seed-demo: cannot create {path}: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let store = Store::new(engine);
    match ndb_studio::seed::seed_demo(&store) {
        Ok(()) => {
            println!("seeded demo dataset into {path}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ndb-studio --seed-demo: {}", e.message());
            ExitCode::FAILURE
        }
    }
}

/// `--merge <out> <src1> <src2> …` — create a fresh database at `out` and copy
/// every record from each source into it, preserving UUIDs and unifying the
/// type/property/role dictionaries by name. One-shot tool; prints counts.
fn run_merge(positionals: &[String], low_memory: bool) -> ExitCode {
    let Some((out, sources)) = positionals.split_first() else {
        eprintln!("--merge needs <out> <src>...");
        return ExitCode::FAILURE;
    };
    if sources.is_empty() {
        eprintln!("--merge needs at least one source database");
        return ExitCode::FAILURE;
    }
    let target_engine = match open_engine(out, true, low_memory) {
        Ok(e) => e,
        Err(msg) => {
            eprintln!("ndb-studio --merge: cannot create {out}: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let target = Store::new(target_engine);
    let (mut tot_e, mut tot_h) = (0usize, 0usize);
    for src in sources {
        let src_engine = match open_engine(src, false, low_memory) {
            Ok(e) => e,
            Err(msg) => {
                eprintln!("ndb-studio --merge: cannot open source {src}: {msg}");
                return ExitCode::FAILURE;
            }
        };
        let source = Store::new(src_engine);
        match target.merge_from(&source) {
            Ok((e, h)) => {
                println!("  {src}: +{e} entities, +{h} hyperedges");
                tot_e += e;
                tot_h += h;
            }
            Err(err) => {
                eprintln!("ndb-studio --merge: copying {src} failed: {}", err.message());
                return ExitCode::FAILURE;
            }
        }
    }
    println!("merged into {out}: {tot_e} entities, {tot_h} hyperedges total");
    ExitCode::SUCCESS
}

fn open_engine(path: &str, new: bool, low_memory: bool) -> Result<SharedEngine, String> {
    let exists = std::path::Path::new(path).join("CURRENT").exists();
    if !new && !exists {
        return Err(format!("no database at {path:?} (use --new to create one)"));
    }
    if low_memory {
        let cfg = EngineConfig::low_memory(DEFAULT_CACHE_BYTES);
        let engine = if new {
            Engine::create_with_config(path, cfg)
        } else {
            Engine::open_with_config(path, cfg)
        }
        .map_err(|e| format!("{e}"))?;
        Ok(SharedEngine::from_engine(engine))
    } else if new {
        SharedEngine::create(path).map_err(|e| format!("{e}"))
    } else {
        SharedEngine::open(path).map_err(|e| format!("{e}"))
    }
}

fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd).arg(url).spawn();
}

fn print_usage() {
    eprintln!(
        "nDB Studio — tables, creative projections, and versioned edits over any nDB\n\n\
         usage:\n  ndb-studio <db-path>            open an existing database\n  \
         ndb-studio --new <db-path>      create a new database\n\n\
         flags:\n  --bind <addr>   listen address (default 127.0.0.1:0)\n  \
         --low-memory    mmap / bounded-RAM mode\n  --no-open       do not launch a browser"
    );
}
