//! nDB command-line client. Talks to `ndb-server` over HTTP/1.1.
//!
//! Subcommands:
//!
//! ```text
//! ndb health
//! ndb read <uuid>
//! ndb commit < records.json    (stdin = CommitRequest JSON)
//! ndb iter
//! ndb flush
//! ndb compact
//! ```
//!
//! Server URL defaults to `http://127.0.0.1:8742`; override with the
//! environment variable `NDB_URL` or the `--url` flag.
//!
//! No external HTTP-client dependency — uses `std::net::TcpStream`
//! directly. JSON in/out via `serde_json`. Pretty-prints responses on
//! stdout.
#![allow(clippy::doc_markdown)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::ExitCode;
use std::time::Duration;

const DEFAULT_URL: &str = "http://127.0.0.1:8742";

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
}

fn usage(out: &mut impl Write) {
    let _ = writeln!(
        out,
        "Usage: ndb [--url URL] <command>\n\
         \n\
         Commands:\n  \
           health                    liveness probe\n  \
           read <uuid>               look up a UUID at the latest snapshot\n  \
           commit                    read CommitRequest JSON from stdin\n  \
           iter                      dump every visible record as JSONL\n  \
           flush                     flush memtable to a new SSTable\n  \
           compact                   full compaction\n\
         \n\
         The server URL defaults to {DEFAULT_URL}; override via --url or NDB_URL.\n"
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
        other => {
            eprintln!("unknown command: {other}");
            return None;
        }
    };
    Some(Args { url, cmd })
}

fn main() -> ExitCode {
    let Some(args) = parse_args() else {
        usage(&mut std::io::stderr());
        return ExitCode::from(2);
    };
    let Some(host_port) = parse_host_port(&args.url) else {
        eprintln!("invalid --url: {}", args.url);
        return ExitCode::from(2);
    };
    let result = match args.cmd {
        Command::Health => do_get(&host_port, "/health"),
        Command::Read { uuid } => do_get(&host_port, &format!("/read/{uuid}")),
        Command::Commit => do_commit(&host_port),
        Command::Iter => do_iter(&host_port),
        Command::Flush => do_post(&host_port, "/flush", ""),
        Command::Compact => do_post(&host_port, "/compact", ""),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn parse_host_port(url: &str) -> Option<String> {
    // Accept "http://host:port" or "host:port".
    let stripped = url.strip_prefix("http://").unwrap_or(url);
    let stripped = stripped.strip_suffix('/').unwrap_or(stripped);
    // Reject anything past the authority.
    if stripped.contains('/') {
        return None;
    }
    Some(stripped.to_owned())
}

fn connect(host_port: &str) -> std::io::Result<TcpStream> {
    let stream = TcpStream::connect(host_port)?;
    stream.set_read_timeout(Some(Duration::from_mins(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    Ok(stream)
}

fn issue(host_port: &str, request: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let mut s = connect(host_port).map_err(|e| format!("connect: {e}"))?;
    s.write_all(request).map_err(|e| format!("write: {e}"))?;
    s.flush().map_err(|e| format!("flush: {e}"))?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "no HTTP header terminator".to_owned())?;
    let head =
        std::str::from_utf8(&buf[..header_end]).map_err(|_| "non-UTF8 HTTP head".to_owned())?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "no status code".to_owned())?;
    Ok((status, buf[header_end + 4..].to_vec()))
}

fn auth_header() -> String {
    std::env::var("NDB_TOKEN").map_or_else(
        |_| String::new(),
        |t| {
            if t.is_empty() {
                String::new()
            } else {
                format!("Authorization: Bearer {t}\r\n")
            }
        },
    )
}

fn do_get(host_port: &str, path: &str) -> Result<(), String> {
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_port}\r\n{}Connection: close\r\n\r\n",
        auth_header()
    );
    let (status, body) = issue(host_port, req.as_bytes())?;
    emit_response(status, &body)
}

fn do_post(host_port: &str, path: &str, body: &str) -> Result<(), String> {
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        auth_header(),
        body.len(),
        body,
    );
    let (status, resp_body) = issue(host_port, req.as_bytes())?;
    emit_response(status, &resp_body)
}

fn do_commit(host_port: &str) -> Result<(), String> {
    let mut body = String::new();
    std::io::stdin()
        .read_to_string(&mut body)
        .map_err(|e| format!("read stdin: {e}"))?;
    // Validate that what's on stdin parses as JSON before sending — gives
    // a friendly error rather than the server's 400.
    let _: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("stdin is not valid JSON: {e}"))?;
    do_post(host_port, "/commit", &body)
}

fn do_iter(host_port: &str) -> Result<(), String> {
    let req = format!(
        "GET /iter HTTP/1.1\r\nHost: {host_port}\r\n{}Connection: close\r\n\r\n",
        auth_header()
    );
    let (status, body) = issue(host_port, req.as_bytes())?;
    if status != 200 {
        return emit_response(status, &body);
    }
    // JSONL — print lines verbatim so this is pipe-friendly. Don't
    // pretty-print here; users can pipe through `jq` if they want.
    let out = std::str::from_utf8(&body).map_err(|_| "non-UTF8 JSONL body".to_owned())?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(out.as_bytes())
        .map_err(|e| format!("stdout: {e}"))?;
    if !out.ends_with('\n') {
        let _ = stdout.write_all(b"\n");
    }
    Ok(())
}

fn emit_response(status: u16, body: &[u8]) -> Result<(), String> {
    // Try to pretty-print JSON; fall back to raw on failure.
    let pretty: Option<String> = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok());
    let printable = pretty.unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    if (200..300).contains(&status) {
        println!("{printable}");
        Ok(())
    } else {
        eprintln!("[{status}]\n{printable}");
        Err(format!("server returned status {status}"))
    }
}
