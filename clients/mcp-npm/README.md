# @n-dimension-database-ndb/mcp

Run the [nDB](https://github.com/goldrag1/nDB) **Model Context Protocol**
server with one command — point Claude, Cursor, or Codex at an nDB database and
give your agent hyperedges, time-travel reads, and paginated iteration.

## Use

```sh
npx @n-dimension-database-ndb/mcp --path ./db
```

Add it to your MCP client config:

```json
{
  "mcpServers": {
    "ndb": { "command": "npx", "args": ["@n-dimension-database-ndb/mcp", "--path", "./db"] }
  }
}
```

## What the agent gets

Tools (each with a JSON input schema, ReBAC-gated, audit-logged):

- `ndb.commit_hyperedge` — create an n-ary relationship as one record.
- `ndb.neighbors` — one-hop traversal of incident hyperedges.
- `ndb.read_as_of` — time-travel read by tx id or wall-clock timestamp.
- `ndb.iter` — cursor-paginated walk of a large database.

Plus MCP resources (`ndb://schema`, `ndb://dictionaries`, `ndb://stats`) and
query-template prompts.

## How it resolves the binary

`bin/ndb-mcp.js` execs the `ndb-mcp-server` binary, passing stdio straight
through (the MCP transport). Resolution order:

1. `$NDB_MCP_SERVER_BIN` — explicit override.
2. The matching per-platform package (`@n-dimension-database-ndb/mcp-linux-x64`,
   `@n-dimension-database-ndb/mcp-linux-arm64`, `@n-dimension-database-ndb/mcp-darwin-arm64`) installed as an
   `optionalDependency` — npm fetches only the one for your host.
3. A local `target/{release,debug}/ndb-mcp-server` — for a dev checkout.
4. `ndb-mcp-server` on `PATH`.

## License

MIT
