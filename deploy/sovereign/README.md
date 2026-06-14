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

**3. An AI agent / MCP client** — point a Streamable-HTTP-capable client at
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
- **Per-agent capability gating**: set `NDB_MCP_PRINCIPAL` (e.g.
  `{"name":"alice","capabilities":["read","iter","vector_search"]}`) so a given
  MCP deployment only exposes the tools that principal allows. Run a second
  `ndb-mcp` unit on another port with a different principal for a read-only tier.
- **Audit**: add `--audit` to the unit's `ExecStart` to append every call to
  `<db>/.audit.jsonl`.

## Scaling out (optional)

Single node by default. For sharding, run `ndb-router` (it speaks the same
`/v1`) in front of N `ndb-server` shards and point `NDB_DATA_UPSTREAM` at the
router instead of a single server. See `docker-compose.sharded.yml`.

## Honest caveats

- **The Rust CLI can't do TLS.** `ndb-client-rust` is plain TCP and rejects
  `https://`. CLI/Rust users need the TS SDK, `curl`, or a private plain-HTTP
  port on a WireGuard/Tailscale net. Browser + MCP + TS SDK are unaffected.
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
