//! nDB command-line client. Thin wrapper over the `ndb_client` library
//! that pretty-prints JSON responses and surfaces server errors.
//!
//! Subcommands:
//!
//! ```text
//! ndb health
//! ndb read <uuid>
//! ndb commit < records.json           # stdin = CommitRequest JSON
//! ndb iter
//! ndb flush
//! ndb compact
//! ndb lookup <prop> <value-json>      # value-json is the tagged-union form
//! ndb vector-search <prop> <k> <metric> < query.json   # stdin = [f32,...]
//! ndb property-lookup <type> <prop> <value-json>
//! ndb property-range <type> <prop> [<low-json>] [<high-json>]
//! ```
//!
//! Server URL defaults to `http://127.0.0.1:8742`; override with the
//! environment variable `NDB_URL` or the `--url` flag. Bearer token comes
//! from `NDB_TOKEN`.
#![allow(clippy::doc_markdown)]

use std::io::{Read, Write};
use std::process::ExitCode;

use ndb_client::{Client, ClientError, DEFAULT_URL};
use ndb_engine::{CommitRequest, JsonValue, QueryRequest, VectorMetric};

struct Args {
    url: String,
    cmd: Command,
}

enum Command {
    Health,
    Read { uuid: String },
    Commit,
    Iter,
    Flush,
    Compact,
    Lookup { property_id: u32, value_json: String },
    VectorSearch { property_id: u32, k: usize, metric: VectorMetric },
    PropertyLookup { type_id: u32, property_id: u32, value_json: String },
    PropertyRange {
        type_id: u32,
        property_id: u32,
        low_json: Option<String>,
        high_json: Option<String>,
    },
    Query { text: Option<String> },
}

fn usage(out: &mut impl Write) {
    let _ = writeln!(
        out,
        "Usage: ndb [--url URL] <command>\n\
         \n\
         Commands:\n  \
           health                                          liveness probe\n  \
           read <uuid>                                     look up a UUID at the latest snapshot\n  \
           commit                                          read CommitRequest JSON from stdin\n  \
           iter                                            dump every visible record as JSONL\n  \
           flush                                           flush memtable to a new SSTable\n  \
           compact                                         full compaction\n  \
           lookup <prop> <value-json>                      find entity by external lookup-key\n  \
           vector-search <prop> <k> <l2|cosine>            k-NN; query vector on stdin as JSON [f32,...]\n  \
           property-lookup <type> <prop> <value-json>      exact match on (type, property, value)\n  \
           property-range <type> <prop> [<low>] [<high>]   range query (low/high are value-json or omitted)\n  \
           query [<text>]                                  execute a query — text positional → POST /query/text;\n  \
                                                             no positional → read QueryRequest JSON from stdin → POST /query\n\
         \n\
         <value-json> is the tagged-union JSON shape, e.g. '{{\"tag\":\"string\",\"value\":\"alice\"}}'\n\
         \n\
         The server URL defaults to {DEFAULT_URL}; override via --url or NDB_URL.\n\
         Bearer token (optional) comes from NDB_TOKEN.\n"
    );
}

fn parse_args() -> Option<Args> {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let mut url = std::env::var("NDB_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--url" | "-u" if i + 1 < argv.len() => {
                url = argv.remove(i + 1);
                argv.remove(i);
            }
            "--help" | "-h" => {
                usage(&mut std::io::stdout());
                return None;
            }
            _ => i += 1,
        }
    }
    let mut argv = argv.into_iter();
    let cmd = match argv.next()?.as_str() {
        "health" => Command::Health,
        "read" => Command::Read { uuid: argv.next()? },
        "commit" => Command::Commit,
        "iter" => Command::Iter,
        "flush" => Command::Flush,
        "compact" => Command::Compact,
        "lookup" => {
            let property_id: u32 = argv.next()?.parse().ok()?;
            let value_json = argv.next()?;
            Command::Lookup { property_id, value_json }
        }
        "vector-search" => {
            let property_id: u32 = argv.next()?.parse().ok()?;
            let k: usize = argv.next()?.parse().ok()?;
            let metric = match argv.next()?.as_str() {
                "l2" | "L2" => VectorMetric::L2,
                "cosine" | "Cosine" => VectorMetric::Cosine,
                other => {
                    eprintln!("metric must be l2 or cosine (got '{other}')");
                    return None;
                }
            };
            Command::VectorSearch { property_id, k, metric }
        }
        "property-lookup" => {
            let type_id: u32 = argv.next()?.parse().ok()?;
            let property_id: u32 = argv.next()?.parse().ok()?;
            let value_json = argv.next()?;
            Command::PropertyLookup { type_id, property_id, value_json }
        }
        "property-range" => {
            let type_id: u32 = argv.next()?.parse().ok()?;
            let property_id: u32 = argv.next()?.parse().ok()?;
            let low_json = argv.next();
            let high_json = argv.next();
            Command::PropertyRange { type_id, property_id, low_json, high_json }
        }
        "query" => Command::Query { text: argv.next() },
        other => {
            eprintln!("unknown command: {other}");
            return None;
        }
    };
    Some(Args { url, cmd })
}

fn parse_value_json(raw: &str) -> Result<JsonValue, String> {
    serde_json::from_str(raw).map_err(|e| format!("invalid value JSON ('{raw}'): {e}"))
}

fn main() -> ExitCode {
    let Some(args) = parse_args() else {
        usage(&mut std::io::stderr());
        return ExitCode::from(2);
    };
    let client = match Client::new(&args.url) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("invalid --url: {e}");
            return ExitCode::from(2);
        }
    };
    let result: Result<(), String> = match args.cmd {
        Command::Health => emit_json(&client.health()),
        Command::Read { uuid } => emit_json(&client.read(&uuid)),
        Command::Commit => run_commit(&client),
        Command::Iter => run_iter(&client),
        Command::Flush => emit_json(&client.flush()),
        Command::Compact => emit_json(&client.compact()),
        Command::Lookup { property_id, value_json } => match parse_value_json(&value_json) {
            Ok(v) => emit_json(&client.lookup_by_key(property_id, v)),
            Err(e) => Err(e),
        },
        Command::VectorSearch { property_id, k, metric } => run_vector_search(&client, property_id, k, metric),
        Command::PropertyLookup { type_id, property_id, value_json } => match parse_value_json(&value_json) {
            Ok(v) => emit_json(&client.property_lookup(type_id, property_id, v)),
            Err(e) => Err(e),
        },
        Command::PropertyRange { type_id, property_id, low_json, high_json } => {
            let low = match low_json {
                Some(s) if s != "-" => Some(match parse_value_json(&s) {
                    Ok(v) => v,
                    Err(e) => return exit_err(&e),
                }),
                _ => None,
            };
            let high = match high_json {
                Some(s) if s != "-" => Some(match parse_value_json(&s) {
                    Ok(v) => v,
                    Err(e) => return exit_err(&e),
                }),
                _ => None,
            };
            emit_json(&client.property_range(type_id, property_id, low, high))
        }
        Command::Query { text } => run_query(&client, text),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn run_commit(client: &Client) -> Result<(), String> {
    let mut body = String::new();
    std::io::stdin()
        .read_to_string(&mut body)
        .map_err(|e| format!("read stdin: {e}"))?;
    let req: CommitRequest =
        serde_json::from_str(&body).map_err(|e| format!("stdin is not a valid CommitRequest: {e}"))?;
    let resp = client.commit(&req).map_err(format_err)?;
    let pretty = serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?;
    println!("{pretty}");
    Ok(())
}

fn run_iter(client: &Client) -> Result<(), String> {
    let records = client.iter().map_err(format_err)?;
    let mut stdout = std::io::stdout().lock();
    for r in records {
        let line = serde_json::to_string(&r).map_err(|e| e.to_string())?;
        writeln!(stdout, "{line}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn run_query(client: &Client, text: Option<String>) -> Result<(), String> {
    let resp = if let Some(text) = text {
        // text path → POST /query/text. Server handles lex + parse + resolve.
        client.query_text(&text).map_err(format_err)?
    } else {
        // stdin path → caller already has a wire-AST.
        let mut body = String::new();
        std::io::stdin()
            .read_to_string(&mut body)
            .map_err(|e| format!("read stdin: {e}"))?;
        let req: QueryRequest =
            serde_json::from_str(&body).map_err(|e| format!("stdin is not a valid QueryRequest: {e}"))?;
        client.query(&req).map_err(format_err)?
    };
    let pretty = serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?;
    println!("{pretty}");
    Ok(())
}

fn run_vector_search(
    client: &Client,
    property_id: u32,
    k: usize,
    metric: VectorMetric,
) -> Result<(), String> {
    let mut body = String::new();
    std::io::stdin()
        .read_to_string(&mut body)
        .map_err(|e| format!("read stdin: {e}"))?;
    let query: Vec<f32> =
        serde_json::from_str(&body).map_err(|e| format!("stdin is not [f32,...]: {e}"))?;
    let hits = client
        .vector_search(property_id, &query, k, metric)
        .map_err(format_err)?;
    let pretty = serde_json::to_string_pretty(&hits).map_err(|e| e.to_string())?;
    println!("{pretty}");
    Ok(())
}

fn emit_json<T: serde::Serialize>(result: &Result<T, ClientError>) -> Result<(), String> {
    match result {
        Ok(v) => {
            let pretty = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
            println!("{pretty}");
            Ok(())
        }
        Err(e) => Err(format_err(e.clone_for_display())),
    }
}

fn format_err(e: ClientError) -> String {
    match e {
        ClientError::Http {
            status,
            error,
            detail,
        } => format!("[{status}] {error}: {detail}"),
        other => other.to_string(),
    }
}

fn exit_err(msg: &str) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::from(1)
}

// `ClientError` is not Clone (its `Io` variant wraps `std::io::Error`),
// so synthesise a display-only clone for the `emit_json` helper above.
trait CloneForDisplay {
    fn clone_for_display(&self) -> ClientError;
}

impl CloneForDisplay for ClientError {
    fn clone_for_display(&self) -> ClientError {
        match self {
            ClientError::Io(e) => ClientError::Parse(format!("io: {e}")),
            ClientError::Http {
                status,
                error,
                detail,
            } => ClientError::Http {
                status: *status,
                error: error.clone(),
                detail: detail.clone(),
            },
            ClientError::Parse(s) => ClientError::Parse(s.clone()),
            ClientError::BadUrl(s) => ClientError::BadUrl(s.clone()),
        }
    }
}
