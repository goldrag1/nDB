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

use std::process::ExitCode;
use std::sync::Arc;

use ndb_engine::{Engine, EngineConfig, SharedEngine};
use ndb_studio::store::Store;
use ndb_studio::{http, identity};

const DEFAULT_CACHE_BYTES: usize = 256 * 1024 * 1024;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path: Option<String> = None;
    let mut bind = "127.0.0.1:0".to_string();
    let mut new = false;
    let mut low_memory = false;
    let mut open = true;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--new" => new = true,
            "--low-memory" => low_memory = true,
            "--no-open" => open = false,
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
            other => path = Some(other.to_string()),
        }
        i += 1;
    }

    let Some(path) = path else {
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
    let primary_name = p.file_name().and_then(|s| s.to_str()).unwrap_or("primary").to_string();
    // New databases live in a sibling "<name>-dbs" directory.
    let root = p.parent().unwrap_or_else(|| std::path::Path::new("."))
        .join(format!("{primary_name}-dbs"));
    let state = Arc::new(http::AppState::new(Arc::new(store), primary_name, root));
    let listener = match http::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ndb-studio: cannot bind {bind}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or_default();
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
        Err(e) => eprintln!("  warning: could not create bootstrap admin: {}", e.message()),
    }
}

fn open_engine(path: &str, new: bool, low_memory: bool) -> Result<SharedEngine, String> {
    let exists = std::path::Path::new(path).join("CURRENT").exists();
    if !new && !exists {
        return Err(format!(
            "no database at {path:?} (use --new to create one)"
        ));
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
