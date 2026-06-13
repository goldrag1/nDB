# nDB Wire Protocol v1

JSON over HTTP/1.1. This is the stable contract the SDKs target. Every route
below is served under the **`/v1`** prefix (e.g. `POST /v1/commit`). The
pre-`/v1` bare paths (`/commit`, …) remain as **deprecated aliases** — see
[COMPATIBILITY.md](COMPATIBILITY.md).

## Conventions

- **Base URL:** `http://host:port` (or `https://` with TLS).
- **Auth:** optional bearer token — `Authorization: Bearer <token>`. Per-route
  capability gating (ReBAC) applies when principals are configured.
- **Content type:** request and response bodies are JSON
  (`application/json`), **except** `POST /v1/query/text`, whose request body is
  the raw query string (`text/plain`), and `GET /v1/arrow/*`, whose response is
  `application/vnd.apache.arrow.stream`.
- **Errors:** non-2xx responses carry
  `{"error": {"code": "<machine_code>", "message": "<human text>"}}`.
  Codes include `unauthorized` (401), `forbidden` (403), `not_found` (404),
  `bad_request` (400), `internal` (500), `unavailable` (503).
- **Capabilities:** `Health`, `Read`, `Iter`, `Commit`, `Flush`, `Compact`,
  `Admin`.

## Routes

### Liveness & observability (capability: Health, unauthenticated)

| Method | Path | Response |
|---|---|---|
| GET | `/v1/health` | `{"status":"ok"}` — always 200 |
| GET | `/v1/ready` | `{"status":"ready","last_tx_id":N}`; 503 while draining |
| GET | `/v1/metrics` | Prometheus text exposition |
| GET | `/v1/status` | `{"sstable_count":N, …}` |

### Writes

**`POST /v1/commit`** — capability `Commit`. Atomically apply a batch of records.

Request:
```json
{
  "records": [
    {
      "kind": "entity",
      "entity_id": "<uuid>",
      "type_id": 1,
      "tx_id_assert": 0,
      "tx_id_supersede": "active",
      "properties": [
        { "prop_id": 10, "value": { "tag": "string", "value": "alice@example.com" } }
      ]
    }
  ]
}
```
Record `kind` is one of `entity`, `hyperedge`, `tombstone`, plus dictionary
records. A hyperedge carries `roles` (entity role-fillers) and optional
`hyperedge_roles` (edges-on-edges) and `properties`. `value.tag` ∈
`string|i64|f64|bool|bytes|entity_ref|vector|decimal|…`.

Response: `{"tx_id": N}` (the assigned transaction id).

### Reads

**`GET /v1/read/<uuid>`** — capability `Read`. Read one entity/hyperedge by id.
Response: `{"outcome": "live", "record": { … }}` (or `"outcome":"tombstoned"` /
`"not_found"`). Missing ids return 200 with a non-`live` outcome, not 404.

**`GET /v1/iter[?snapshot=<tx|ts>]`** — capability `Iter`. Stream all visible
records at a snapshot. Response is **JSONL** (`application/jsonl`): one JSON
record per line, not a JSON array. Internal metadata records (tx-timestamp,
retention) are filtered out. `snapshot` optionally pins an `as_of` read.
(Cursor pagination is exposed via the MCP `ndb.iter` tool, not this route.)

**`POST /v1/query`** — capability `Read`. Run a wire-AST query (the
machine-readable form). Request: a `QueryRequest` JSON object. Response:
`{"columns": ["?n", …], "rows": [[…], …]}`.

**`POST /v1/query/text`** — capability `Read`. Run query *source text*. The
server lexes/parses/resolves against its dictionary, so the client ships no
parser. **Request body is the raw query string**, e.g.
`match customer(name: ?n) return ?n limit 10`. Response: same
`{"columns", "rows"}` shape as `/v1/query`.

**`POST /v1/query/explain`** — capability `Read`. Returns the query plan for the
given request without executing it.

**`POST /v1/lookup`** — capability `Read`. Exact lookup by `(type_id, key)`.
**`POST /v1/property_lookup`** — capability `Read`. Entities with a property = value.
**`POST /v1/property_range`** — capability `Read`. Range query on `(type_id,
property_id)` with inclusive `low`/`high` (either may be null = unbounded).
**`POST /v1/vector_search`** — capability `Read`. kNN over a registered vector
property: `{type_id, property_id, query: [f32…], k}` → ranked ids + distances.
**`POST /v1/traverse`** — capability `Read`. Graph traversal from a seed id.
**`POST /v1/subscribe`** — capability `Read`. Long-poll for commits past a watermark.

### Admin & lifecycle

| Method | Path | Capability | Purpose |
|---|---|---|---|
| POST | `/v1/flush` | Flush | Flush memtable → SSTable; returns counts |
| POST | `/v1/compact` | Compact | Force a compaction; returns merge stats |
| POST | `/v1/replicate` | Admin | Follower pull (log-shipping) |
| POST | `/v1/admin/shutdown` | Admin | Graceful shutdown |

### GPU / data egress (capability: Read)

| Method | Path | Response |
|---|---|---|
| GET | `/v1/arrow/export[?batch_rows=N]` | Arrow IPC stream of all records |
| GET | `/v1/arrow/vectors?type_id=T&property_id=P` | `primary_id + FixedSizeList<Float32,dim>` batches |
| GET | `/v1/arrow/edge_index` | hyperedge incidence list for GNN frameworks |

## Versioning

Within protocol major **v1**, only additive, backward-compatible changes are
made. A breaking change ships as **`/v2`** served *alongside* `/v1`, so a
deployed client never breaks on a server upgrade. See
[COMPATIBILITY.md](COMPATIBILITY.md) for the full policy across the engine,
protocol, on-disk format, and SDK.
