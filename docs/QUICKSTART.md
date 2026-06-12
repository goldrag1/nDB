# nDB Quickstart (application developers)

A 5-minute path from an empty checkout to reading and writing data over
the network. For the data model (hyperedges, time-travel) see the
[white paper](nDB-whitepaper.md); for the query language see §12 of the
[design spec](superpowers/specs/2026-05-27-nDB-hypergraph-design.md).

## 1. Run the server

```sh
# Build + start the HTTP server on a database directory (created if absent).
cargo run --release -p ndb-server -- --path ./mydb --bind 127.0.0.1:8742
```

The server exposes JSON over HTTP/1.1. Operational endpoints:

| Endpoint | Purpose |
|---|---|
| `GET /health` | liveness — always `200 {"status":"ok"}` |
| `GET /ready`  | readiness — `503` while shutting down / engine unavailable |
| `GET /metrics`| Prometheus text (`ndb_requests_total`, `ndb_auto_compactions_total`, …) |
| `GET /status` | `{"sstable_count", …}` |

```sh
curl -s http://127.0.0.1:8742/health
# {"status":"ok"}
```

Compaction runs automatically in the background once the SSTable count
crosses a threshold (default 8, checked every 30s) — no cron needed. Tune
or disable it with `Server::with_auto_compaction(threshold, interval)`.

## 2. Talk to it from Rust

```toml
# Cargo.toml
[dependencies]
ndb-client-rust = { path = "…/crates/ndb-client-rust" }
```

```rust
use std::time::Duration;
use ndb_client::Client;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Retries are opt-in: GET requests retry fully (transport + 502/503/504);
    // writes retry only the connection, so a commit is never double-applied.
    let client = Client::new("http://127.0.0.1:8742")?
        .with_retries(3, Duration::from_millis(100))
        .with_token(std::env::var("NDB_TOKEN").unwrap_or_default());

    // Liveness.
    let h = client.health()?;
    println!("server status: {}", h.status);

    // Query with the §12 query language (text form). Variables are `?name`;
    // writes use create/set/merge/delete clauses, reads return tuples.
    let result = client.query_text("match customer(name: ?n) return ?n limit 10")?;
    println!("columns: {:?}, rows: {}", result.columns, result.rows.len());

    Ok(())
}
```

Other client methods: `read(uuid)`, `iter()`, `lookup_by_key(...)`,
`property_lookup(...)`, `property_range(...)`, `vector_search(...)`,
`traverse(...)`, `query(&QueryRequest)`, plus admin `flush()` / `compact()`.
Timeouts default to 60s read / 30s write and are tunable via
`with_read_timeout` / `with_write_timeout`.

## 3. Or use the CLI

```sh
cargo run --release -p ndb-cli -- --url http://127.0.0.1:8742 health
```

The `ndb` binary wraps the same client for shell / scripting use.

## 4. Auth, TLS, encryption (production)

- **Auth:** start the server with a bearer token; clients send it via
  `with_token` / the `NDB_TOKEN` env var. Per-tool access is gated by
  ReBAC capabilities.
- **TLS:** `Server::with_tls_pem(cert, key)` then `run_tls(addr)`.
- **At-rest encryption:** set `NDB_ENC_KEY` before `open`/`create`; the
  engine encrypts new files (AES-GCM-256) and refuses to open on a key
  mismatch.

## 5. Embed the engine directly (no server)

For in-process use, depend on `ndb-engine` and drive `Engine` /
`WriteTxn` / `snapshot_iter_streaming` directly — the server is just a
network skin over the same library. See `crates/ndb-engine/examples/`.
