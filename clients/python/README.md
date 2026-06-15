# ndb-client (Python)

A pure-Python HTTP client for [nDB](https://github.com/goldrag1/nDB), the
n-dimensional hypergraph database. Zero non-stdlib dependencies — uses
`urllib` and `json` under the hood.

## Install

```sh
pip install ndb-client
```

## Quick start

Start `ndb-server` somewhere first:

```sh
cargo run -p ndb-server -- --path /tmp/mydb
```

Then talk to it from Python:

```python
from ndb_client import Client

ndb = Client(base_url="http://127.0.0.1:8742")

# Liveness check.
ndb.health()  # → {"status": "ok"}

# Commit an entity.
resp = ndb.commit([{
    "kind": "entity",
    "entity_id": "01928f2a-3d4e-7000-8000-000000000001",
    "type_id": 1,
    "tx_id_assert": 0,
    "tx_id_supersede": "active",
    "properties": [
        {"prop_id": 10, "value": {"tag": "string", "value": "alice@example.com"}},
    ],
}])
print("committed at tx", resp["tx_id"])

# Stream every visible record.
for record in ndb.iter():
    print(record)

# Vector k-NN.
hits = ndb.vector_search(property_id=42, query=[0.1, 0.2, 0.3], k=5, metric="l2")

# Admin endpoints.
ndb.flush()
ndb.compact()
```

## Auth

```python
ndb = Client(base_url="https://ndb.example.com", token="my-bearer-token")
```

The client also honors `NDB_TOKEN` in the environment as a default.

## TLS

`https://` URLs use Python's stdlib `ssl` module. For self-signed certs
in development, pass `verify_ssl=False`:

```python
ndb = Client(base_url="https://localhost:8742", verify_ssl=False)
```

## Text queries

`query_text` runs the human-readable query language (the server resolves
names → ids), mirroring the TS SDK's `queryText`:

```python
res = ndb.query_text("match (c:Commit) return c.message limit 5")
print(res["columns"], res["rows"])
```

## Arrow → pandas / polars (optional)

Install the `arrow` extra, then `arrow_export()` pulls **server-side** Arrow
IPC (the `ndb-arrow` crate's columnar projection) into a `pyarrow.Table` —
the efficient on-ramp to the PyData stack:

```sh
pip install 'ndb-client[arrow]'
```

```python
tbl = ndb.arrow_export()       # pyarrow.Table (server does the projection)
df  = tbl.to_pandas()          # pandas
import polars as pl; pl.from_arrow(tbl)   # polars (zero-copy)
```

`iter_arrow()` is also available (client-side projection over the JSONL
`/iter` stream) but `arrow_export()` is preferred — it offloads the
columnar work to the server and transfers Arrow, not JSON.

## Five lines against a live nDB

```python
from ndb_client import Client
db = Client("https://ndb-pingora.nextstar-erp.com")   # public sovereign demo (tailnet)
print(db.health())                                    # {'status': 'ok'}
tbl = db.arrow_export()                                # real agent-memory records
print(tbl.num_rows, "records"); print(tbl.to_pandas().head())
```

## Status

Surface mirrors the TS SDK + `ndb` CLI: `health`, `commit`, `read`,
`iter`, `flush`, `compact`, `lookup_by_key`, `vector_search`,
`property_lookup`, `property_range`, `traverse`, `query`, `query_text`,
plus `arrow_export` / `iter_arrow`. See `crates/ndb-mcp-server` for the
canonical tool shapes and `crates/ndb-engine/src/wire.rs` for the wire
JSON.
