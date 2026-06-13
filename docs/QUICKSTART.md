# nDB Quickstart

Two ≤5-minute paths: one for **application developers**, one for **AI coding
agents**. For the data model (hyperedges, time-travel) see the
[white paper](nDB-whitepaper.md); for the wire contract see
[PROTOCOL.md](PROTOCOL.md); for upgrade/durability guarantees see
[COMPATIBILITY.md](COMPATIBILITY.md).

---

## Path A — Application developer (TypeScript)

### 1. Run the server (Docker — no toolchain needed)

```sh
docker run -p 8742:8742 -v ndb-data:/data ghcr.io/goldrag1/ndb
curl -s http://127.0.0.1:8742/v1/health        # {"status":"ok"}
```

Prefer a binary? Grab `ndb-<platform>.tar.gz` from the
[latest release](https://github.com/goldrag1/nDB/releases) and run
`./ndb-server --path ./mydb --bind 127.0.0.1:8742`. From source:
`cargo run --release -p ndb-server -- --path ./mydb --bind 127.0.0.1:8742`.

Compaction runs automatically once the SSTable count crosses a threshold — no
cron. Operational endpoints: `/v1/health` (liveness), `/v1/ready` (readiness),
`/v1/metrics` (Prometheus), `/v1/status`.

### 2. Talk to it from TypeScript

```sh
npm i @n-dimension-database-ndb/client
```

```ts
import { NdbClient } from "@n-dimension-database-ndb/client";

const db = new NdbClient("http://127.0.0.1:8742", { retries: 3 });

console.log((await db.health()).status);             // "ok"

const id = crypto.randomUUID();
const { tx_id } = await db.commit({
  records: [{
    kind: "entity",
    entity_id: id,
    type_id: 1,
    tx_id_assert: 0,
    tx_id_supersede: "active",
    properties: [{ prop_id: 10, value: { tag: "string", value: "alice@example.com" } }],
  }],
});

const r = await db.read(id);                          // { outcome: "live", … }
const q = await db.queryText("match entity() return ?x limit 10");
console.log(q.columns, q.rows);
```

Runs in Node ≥18, browsers, Deno, and edge runtimes — anything with `fetch`.
Full API + retry semantics: [`clients/ts/README.md`](../clients/ts/README.md).

### 3. Or embed the engine directly (no server)

For in-process use, depend on `ndb-engine` and drive `Engine` / `WriteTxn` /
`snapshot_iter_streaming` directly — the server is just a network skin over the
same library. See `crates/ndb-engine/examples/`. Rust and Python network
clients also ship (`crates/ndb-client-rust`, the Python client, and `ndb-cli`).

---

## Path B — AI coding agent (MCP)

nDB speaks the **Model Context Protocol** — the same protocol Claude, Cursor,
and Codex use for tools. Point your agent at a database and it can create and
traverse n-ary relationships (hyperedges), do time-travel reads, and page
through the data.

### 1. Run the MCP server

```sh
npx @n-dimension-database-ndb/mcp --path ./db
```

(Or run the binary from a release / `cargo run -p ndb-mcp-server -- --path ./db`.)

### 2. Connect your agent

Add nDB to your MCP client config (Claude Desktop / Cursor / Codex):

```json
{
  "mcpServers": {
    "ndb": { "command": "npx", "args": ["@n-dimension-database-ndb/mcp", "--path", "./db"] }
  }
}
```

### 3. What the agent gets

Tools (each with a JSON input schema, ReBAC-gated, audit-logged):

- `ndb.commit_hyperedge` — create an n-ary relationship as one record
  (entity and/or hyperedge role-fillers).
- `ndb.neighbors` — one-hop traversal of incident hyperedges.
- `ndb.read_as_of` — time-travel read by tx id or wall-clock timestamp.
- `ndb.iter` — cursor-paginated walk of an arbitrarily large database.

Plus MCP **resources** (`ndb://schema`, `ndb://dictionaries`, `ndb://stats`)
so the agent can discover the schema without trial-and-error, and **prompts**
(query templates) for common explorations.

---

## Auth, TLS, encryption (production)

- **Auth:** start the server with a bearer token; clients send it
  (`new NdbClient(url, { token })` / `NDB_TOKEN`). Per-route ReBAC gating.
- **TLS:** `Server::with_tls_pem(cert, key)` then `run_tls(addr)`.
- **At-rest encryption:** set `NDB_ENC_KEY` before `open`/`create`; AES-GCM-256,
  refuses to open on a key mismatch.

See [PRODUCTION.md](PRODUCTION.md) for the full operations guide.
