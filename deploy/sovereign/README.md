# nDB Sovereign Deploy — Pingora TLS edge + nDB, no tunnel

A self-owned, all-Rust deployment of nDB that other developers (and their AI
agents) reach over plain HTTPS. **No Cloudflare tunnel, no nginx, no certbot
plugin** — Pingora terminates TLS, Let's Encrypt issues the cert, three small
systemd services run the stack.

This is the "Option 2 / sovereign edge" from the design discussion. Use it on a
**dedicated nDB box** (it binds :443). If you're co-hosting with something else
that owns :443, keep your tunnel and run Pingora internally instead.

## Architecture

```
            DNS A record  ndb.example.com ──► your VPS
                                   │
                              :443 │ TLS  (Pingora: ndb-edge)
                                   ▼
              ┌───────────── path route ─────────────┐
              │                                       │
   /mcp*  ──► ndb-mcp-server --http  :9000     /v1*, /health ──► ndb-server :8742
   (AI agents, Streamable HTTP MCP)            (data API, /v1 wire protocol)
              │                                       │
              └────────────── ndb-engine ────────────┘
                       (one binary each, single-writer)
```

TLS stops at Pingora; both upstreams are plain HTTP on `127.0.0.1`. One shared
bearer token (`NDB_TOKEN`) gates `/v1` and `POST /mcp`; `/health` stays open for
liveness probes.

### Unified single-engine launch (recommended)

The two upstreams above can be **separate processes over separate databases** —
in which case a write via `/mcp` is *not* visible via `/v1`. To serve both
surfaces from **one engine in one process**, run `ndb-mcp-server` with both
`--http` and `--v1-bind`:

```bash
ndb-mcp-server --path /var/lib/ndb/data \
  --http 127.0.0.1:9000 \
  --v1-bind 127.0.0.1:8742
```

Now `/mcp` and `/v1` share a single `Arc<RwLock<Engine>>` (single-writer
preserved by the lock): a `tools/call ndb.commit_entity` over `/mcp` is
instantly readable via `GET /v1/read/:uuid`. Auth parity follows the env: with
`NDB_TOKEN` both surfaces are fully locked; with only `NDB_WRITE_TOKEN`, `/v1`
is read-only (writes go through `/mcp`); with neither, `/v1` is open. The
`mcp_write_is_instantly_readable_via_v1_on_one_engine` integration test is the
gate.

## What's in this directory

| Path | What |
|---|---|
| `ndb-edge/` | The Pingora TLS edge (standalone crate; `cargo check` clean vs pingora 0.8.1). |
| `systemd/ndb-server.service` | nDB data API (`/v1`), localhost:8742. |
| `systemd/ndb-mcp.service` | MCP Streamable-HTTP server (`/mcp`), localhost:9000. |
| `systemd/ndb-edge.service` | Pingora edge, :443, `CAP_NET_BIND_SERVICE`. |
| `scripts/obtain-certs.sh` | Let's Encrypt cert + auto-renew→restart hook. |
| `scripts/install.sh` | Create user/dirs, install binaries + units. |
| `ndb.env.example` | Shared env (token, TLS paths, upstreams). |

## Prerequisites

- An Ubuntu VPS you control, with a public IP.
- A domain with a **DNS A record** → that IP (e.g. `ndb.example.com`).
- Inbound **:80** (cert challenge, briefly) and **:443** (serving) open.
- Rust toolchain on the box (or build elsewhere and `scp` the binaries).

## Deploy (start to finish)

```bash
# 0. clone + enter the repo on the VPS
git clone https://github.com/goldrag1/nDB && cd nDB

# 1. build the three binaries
cargo build --release -p ndb-server -p ndb-mcp-server
( cd deploy/sovereign/ndb-edge && cargo build --release )

# 2. install user, dirs, binaries, systemd units
sudo deploy/sovereign/scripts/install.sh

# 3. get a TLS cert (needs :80 open + DNS pointing here)
sudo deploy/sovereign/scripts/obtain-certs.sh ndb.example.com you@example.com

# 4. set a real shared token
sudo sed -i "s/replace-with-openssl-rand-hex-32/$(openssl rand -hex 32)/" /etc/ndb/ndb.env
sudo cat /etc/ndb/ndb.env | grep NDB_TOKEN   # copy this — devs need it

# 5. start everything
sudo systemctl enable --now ndb-server ndb-mcp ndb-edge
sudo systemctl status ndb-edge --no-pager
```

## Verify

```bash
# liveness (no token)
curl -s https://ndb.example.com/health
# → {"status":"ok"}

# data API (token required)
curl -s https://ndb.example.com/v1/health -H "Authorization: Bearer $TOKEN"

# MCP over HTTPS (token required)
curl -s https://ndb.example.com/mcp -H "Authorization: Bearer $TOKEN" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | head -c 200
```

## How other developers connect

**1. Raw HTTP / curl** — `POST https://ndb.example.com/mcp` with the bearer
token, JSON-RPC body. Or `/v1/*` for the data API.

**2. The TS SDK** (data API, works over TLS):
```js
import { NdbClient } from "@n-dimension-database-ndb/client";
const db = new NdbClient("https://ndb.example.com", { token: TOKEN });
await db.health();
```

**3. The Rust CLI / SDK** (TLS built in — rustls + bundled roots):
```bash
ndb --url https://ndb.example.com health      # https:// → TLS, port 443
# or in code: ndb_client::Client::new("https://ndb.example.com")
#             .with_token(token)
```

**4. An AI agent / MCP client** — point a Streamable-HTTP-capable client at
`https://ndb.example.com/mcp`. For stdio-only clients (some Claude/Cursor/Codex
setups), bridge with `mcp-remote`:
```json
{
  "mcpServers": {
    "ndb": {
      "command": "npx",
      "args": ["mcp-remote", "https://ndb.example.com/mcp",
               "--header", "Authorization: Bearer ${NDB_TOKEN}"]
    }
  }
}
```
The agent now lists nDB's tools (`ndb.vector_search`, `ndb.commit_hyperedge`,
`ndb.neighbors`, …) and reasons over the database remotely.

## Multi-developer access

- **Shared token** (`NDB_TOKEN`): simplest — one secret, everyone sends it.
- **Per-dev tokens + capability scoping (recommended for teams):** point
  `NDB_MCP_PRINCIPALS` at a registry file mapping each dev's bearer token to a
  principal with its own capability set. Each HTTP request's token resolves to
  that principal and the call is gated by its capabilities — and **audited under
  the dev's name** (the registry forces auditing on). One endpoint, many devs,
  least-privilege per dev:

  ```json
  // /etc/ndb/principals.json   (chmod 600)
  {
    "principals": {
      "tok-alice-9f3…": {"name": "alice", "capabilities": ["health","read","iter","commit"]},
      "tok-bob-7a1…":   {"name": "bob",   "capabilities": ["health","read","iter"]}
    }
  }
  ```
  ```ini
  # in the ndb-mcp unit's EnvironmentFile (/etc/ndb/ndb.env)
  NDB_MCP_PRINCIPALS=/etc/ndb/principals.json
  ```
  Now `bob`'s token can read but a `commit` returns a JSON-RPC error naming the
  missing `commit` capability, while `alice`'s token commits — and both calls
  land in `<db>/.audit.jsonl` attributed to `alice` / `bob`. Capabilities:
  `health`, `read`, `iter`, `commit`, `flush`, `compact`, `admin` (wildcard).
- **Single-principal gate:** `NDB_MCP_PRINCIPAL` (one JSON principal) gates a
  whole deployment — handy for a read-only tier on a second port.
- **Audit**: `--audit` / `NDB_AUDIT=1` append every call to `<db>/.audit.jsonl`
  (`{ts_us, principal, method, path, status, failure}` per line); the principals
  registry turns this on automatically.

## Scaling out (optional)

Single node by default. For sharding, run `ndb-router` (it speaks the same
`/v1`) in front of N `ndb-server` shards and point `NDB_DATA_UPSTREAM` at the
router instead of a single server. See `docker-compose.sharded.yml`.

## Configuring a vector property (no env knob)

Vector search needs the property registered. The first-class way is the MCP
tool — it persists (survives restarts) and needs no env var:

```bash
curl -s https://ndb.example.com/mcp -H "Authorization: Bearer $TOKEN" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
        "name":"ndb.register_vector_property","arguments":{"property_id":20}}}'
```

The id is written to `<db>/.vector_props` and re-registered (HNSW rebuilt from
the durable records) on the next open. Name the type/role/property slots so the
catalog reads cleanly with `ndb.define_schema` (`{types,roles,properties}` of
`{id,name}`). The legacy `NDB_VECTOR_PROP=<ids>` env still works but is
deprecated — at boot it's folded into the same sidecar and logs a notice.

## systemd gotcha: `${VAR}` is NOT shell-expanded in `ExecStart`

systemd does **not** run `ExecStart` through a shell, so common shell idioms
silently fail:

- `${NDB_TOKEN}` only expands if `NDB_TOKEN` is set via `Environment=` /
  `EnvironmentFile=` **in the same unit** — and only as a standalone token.
  `$VAR` spliced mid-word, `$(command)` substitution, `|` pipes, `&&`, `*`
  globs, and `~` are all passed through **literally** (or rejected).
- Worse, an *undefined* `${VAR}` expands to the **empty string** with no error
  — so a service can start "successfully" with a blank token (i.e. wide open)
  if the `EnvironmentFile` path was wrong or the key was misspelled.

Two safe patterns:

```ini
# A) Pass real env to the program; let the program read it (what nDB does).
EnvironmentFile=/etc/ndb/ndb.env
ExecStart=/usr/local/bin/ndb-mcp-server --path /var/lib/ndb/data --http 127.0.0.1:9000
# ndb-mcp-server reads NDB_TOKEN / NDB_WRITE_TOKEN / NDB_VECTOR_PROP from the env.

# B) If you truly need shell features, invoke a shell explicitly.
ExecStart=/bin/sh -c 'exec /usr/local/bin/ndb-mcp-server --path "$NDB_DATA" --http "$NDB_BIND"'
```

Prefer (A): keep secrets/config in `EnvironmentFile` and have the binary read
them, rather than interpolating into the command line. Verify what actually
launched with `systemctl show -p ExecStart,Environment <unit>` and confirm the
token is non-empty in the running process (`systemctl show -p Environment`).

## Honest caveats

- **TLS is fully covered, every client path.** `ndb-client-rust` and the `ndb`
  CLI speak TLS to `https://` endpoints (rustls + ring + bundled Mozilla
  `webpki-roots`, default port 443 — see the `https_defaults_to_443_and_tls`
  test), `ndb-server` can also terminate TLS itself (`--tls-cert/--tls-key`),
  and the edge terminates TLS for `/mcp` + `/v1`. So curl, TS SDK, Rust CLI/SDK,
  browser, and MCP all work over HTTPS — no plain-HTTP side door needed. (The
  MCP `--http` transport is plain HTTP *by design*: the edge wraps it in TLS.)
- **Runtime TLS not exercised here.** `ndb-edge` is verified to `cargo check`
  against pingora 0.8.1; the live handshake needs a real cert + :443, so test it
  on the box after step 5 (the `curl https://…/health` above is the gate).
- **Pingora API drift.** If a future pingora bump breaks the build, the offender
  is almost always `add_tls_with_settings` / `TlsSettings` / `ProxyHttp` — check
  `ndb-edge/src/main.rs` against docs.rs for the resolved version.
- **Alternative to a custom proxy:** Cloudflare's [`river`](https://github.com/memorysafety/river)
  is a config-driven Pingora binary (no Rust to maintain). Viable once its
  path-routing + file-cert support cover this shape; today the ~60-line
  `ndb-edge` crate is the more predictable path.
