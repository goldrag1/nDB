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

## Arrow interop (optional)

Install with the `arrow` extra to get `iter_arrow()`, which materializes
the record stream as a `pyarrow.RecordBatch`:

```sh
pip install 'ndb-client[arrow]'
```

```python
batch = ndb.iter_arrow()  # pyarrow.RecordBatch
import polars as pl
df = pl.from_arrow(batch)
```

The wire format mirrors `ndb_engine::wire`'s tagged-union JSON shapes;
the Arrow projection is best-effort over the JSON stream (the same shape
the `ndb-arrow` Rust crate produces, but bridged through Python instead
of consumed from a Rust binary directly).

## Status

v1.0 — surface mirrors the `ndb` CLI: `health`, `commit`, `read`,
`iter`, `flush`, `compact`, `lookup_by_key`, `vector_search`,
`property_lookup`, `property_range`. See `crates/ndb-mcp-server` for the
canonical tool shapes and `crates/ndb-engine/src/wire.rs` for the wire
JSON.
