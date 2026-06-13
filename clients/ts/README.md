# @ndb/client

Thin, typed TypeScript client for the [nDB](https://github.com/goldrag1/nDB)
wire protocol **v1**. Zero runtime dependencies. Runs anywhere `fetch` exists —
Node ≥18, browsers, Deno, and edge runtimes.

It mirrors the Rust client surface and targets the `/v1` HTTP API documented in
[`docs/PROTOCOL.md`](../../docs/PROTOCOL.md). Data-durability and upgrade
guarantees: [`docs/COMPATIBILITY.md`](../../docs/COMPATIBILITY.md).

## Install

```sh
npm i @ndb/client
```

## Use

```ts
import { NdbClient } from "@ndb/client";

const db = new NdbClient("http://127.0.0.1:8742", {
  token: process.env.NDB_TOKEN, // optional bearer token
  retries: 3,                   // GET retries fully; writes retry connection-only
});

// Liveness
console.log((await db.health()).status); // "ok"

// Write a record (see docs/PROTOCOL.md for the record shape)
const { tx_id } = await db.commit({
  records: [{
    kind: "entity",
    entity_id: crypto.randomUUID(),
    type_id: 1,
    tx_id_assert: 0,
    tx_id_supersede: "active",
    properties: [{ prop_id: 10, value: { tag: "string", value: "alice@example.com" } }],
  }],
});

// Read it back
const r = await db.read("…uuid…");        // { outcome: "live", record: {…} }

// Query by source text (the server parses + resolves names)
const res = await db.queryText("match customer(name: ?n) return ?n limit 10");
console.log(res.columns, res.rows);

// Walk all records at a snapshot (server streams JSONL → parsed to an array)
for (const rec of await db.iter()) { /* … */ }
```

## API

Reads: `health()`, `read(uuid)`, `iter({snapshot?})`, `query(req)`,
`queryText(text)`, `lookup(req)`, `vectorSearch(req)`, `propertyLookup(req)`,
`propertyRange(req)`, `traverse(req)`.
Writes: `commit(req)`, `flush()`, `compact()`.

Errors are thrown as `NdbError` with `.status`, `.code`, and `.message`.

### Retry semantics

Matching the Rust client: `GET` requests retry on transport errors **and**
`502/503/504` responses; writes retry **only** when the connection failed
before any response arrived — so a commit is never applied twice.

## Develop

```sh
npm run typecheck
cargo build -p ndb-server      # the integration test spawns this binary
npm test                       # spawns ndb-server, exercises the client over /v1
npm run build                  # → dist/
```

Set `NDB_SERVER_BIN` to point the test at a specific server binary; it defaults
to `target/debug/ndb-server` at the repo root.

## License

MIT
