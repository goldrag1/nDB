# Build your first app on nDB

A 5-minute, hands-on tour that leads with the three things nDB does that a
relational or document store makes you fight for: **N-ary hyperedges**,
**time-travel**, and **co-located vectors**. By the end you'll have written
facts, walked a relationship, queried the past, and done a semantic search —
all over one engine.

You'll use the Python client; the same shapes work from the TS SDK, the `ndb`
CLI, `curl`, or an MCP agent.

## 0. Get a server (under a minute)

**Option A — one command, local:**

```sh
deploy/sovereign/scripts/install.sh --local        # builds + runs on :9000 (/mcp) + :8742 (/v1)
```

**Option B — point at the public sovereign demo** (read-only-friendly, tailnet):

```python
BASE = "https://ndb-pingora.nextstar-erp.com"       # /v1 data API + /mcp
```

Install the client:

```sh
pip install 'ndb-client[arrow]'
```

```python
from ndb_client import Client
db = Client("http://127.0.0.1:8742")                 # or BASE from Option B
db.health()                                          # {'status': 'ok'}  ← first query!
```

## 1. The data model in three nouns

- **Entity** — a thing. A UUID, a `type_id`, and a bag of properties.
- **Hyperedge** — *one* fact connecting **any number** of entities by named
  **roles**. Not a row, not a pair of edges — a single first-class N-ary fact.
- **Property** — a tagged value (`{"tag":"string","value":"…"}`, `i64`, `f64`,
  `vector`, …) on an entity or a hyperedge.

We'll model a tiny dev-activity graph: people, commits, and the N-ary
"who committed what, touching which files" fact.

A quick schema-naming call makes the catalog readable (ids → names). Over MCP:

```python
# (skip if you're only using /v1 — naming is an MCP tool)
# tools/call ndb.define_schema  types=[{id:1,Person},{id:3,Commit},{id:2,File}]
#                               roles=[{id:1,author},{id:2,touches}]  properties=[{id:10,name},{id:13,message}]
```

## 2. Write entities

```python
import uuid
def eid(): return str(uuid.uuid4())

ada   = eid(); kay = eid()
fileA = eid(); fileB = eid()
commit1 = eid()

db.commit([
  {"kind":"entity","entity_id":ada,  "type_id":1,"tx_id_assert":0,"tx_id_supersede":"active",
   "properties":[{"prop_id":10,"value":{"tag":"string","value":"Ada"}}]},
  {"kind":"entity","entity_id":kay,  "type_id":1,"tx_id_assert":0,"tx_id_supersede":"active",
   "properties":[{"prop_id":10,"value":{"tag":"string","value":"Kay"}}]},
  {"kind":"entity","entity_id":fileA,"type_id":2,"tx_id_assert":0,"tx_id_supersede":"active",
   "properties":[{"prop_id":12,"value":{"tag":"string","value":"engine.rs"}}]},
  {"kind":"entity","entity_id":fileB,"type_id":2,"tx_id_assert":0,"tx_id_supersede":"active",
   "properties":[{"prop_id":12,"value":{"tag":"string","value":"http.rs"}}]},
  {"kind":"entity","entity_id":commit1,"type_id":3,"tx_id_assert":0,"tx_id_supersede":"active",
   "properties":[{"prop_id":13,"value":{"tag":"string","value":"bounded concurrency"}}]},
])
```

## 3. The hyperedge — the part tables can't do cleanly

One commit, by Ada, touching **two** files. In SQL that's a row plus a
`commit_files` junction table. In a graph DB it's three binary edges. In nDB
it's **one record**:

```python
db.commit([{
  "kind":"hyperedge","hyperedge_id":eid(),"type_id":100,   # 100 = CommitEvent
  "tx_id_assert":0,"tx_id_supersede":"active",
  "roles":[
    {"role_id":4,"entity_id":commit1},   # 4 = commit
    {"role_id":1,"entity_id":ada},        # 1 = author
    {"role_id":2,"entity_id":fileA},      # 2 = touches
    {"role_id":2,"entity_id":fileB},      #     …same role, repeated — N-ary
  ],
  "properties":[],
}])
```

Walk it back — "what does Ada connect to?" — in one server-side hop:

```python
db.traverse(ada, [{"hyperedge_type_id":100}])   # → [commit1, fileA, fileB, …]
```

Or in the query language:

```python
db.query_text('match (c:Commit)<-[:commit]-(e:CommitEvent)-[:author]->(p:Person) '
              'return c.message, p.name')
```

## 4. Time-travel — query the past for free

Every write is an MVCC assertion; nothing is overwritten. A read pins a
snapshot, so "what did this look like at tx T / time T?" is a normal read, not
an audit subsystem.

```python
snap = db.commit([...])["tx_id"]      # remember a point in time
# ... more writes happen ...
db.read(commit1)                       # latest
# time-travel via the query language or read_as_of (MCP): as-of that snapshot
db.query_text(f'match (c:Commit) as of {snap} return c.message')
```

The cost of the historical read is the same as the current one — that's the
point of MVCC + snapshot reads.

## 5. Vectors — semantic search, co-located with the graph

Register a property as vector-indexed (once; it persists), attach embeddings,
and k-NN lives next to your relationships — no separate vector DB to sync.

```python
# register prop 20 as vector (MCP tool: ndb.register_vector_property {property_id:20})
# then attach an embedding to a commit and search:
db.commit([{"kind":"entity","entity_id":eid(),"type_id":3,"tx_id_assert":0,
            "tx_id_supersede":"active",
            "properties":[{"prop_id":20,"value":{"tag":"vector","value":[0.1,0.2,0.3]}}]}])

db.vector_search(property_id=20, query=[0.1,0.2,0.25], k=5, metric="cosine")
# → [(entity_id, distance), …]  — the same engine that holds the hyperedges
```

If the server runs the embedding model (the demo box does), the MCP
`ndb.search_text` tool embeds a natural-language query for you and returns the
nearest records — "recall memories like this".

## 6. Analytics on-ramp

Pull the whole store as Arrow and hand it to pandas / polars / DuckDB:

```python
tbl = db.arrow_export()        # server-side columnar projection, Arrow IPC
df  = tbl.to_pandas()
```

## Where to go next

- **Wire shapes:** `crates/ndb-engine/src/wire.rs` (tagged values, records).
- **Every MCP tool + schema:** `crates/ndb-mcp-server` (`tools/list`).
- **Deploy your own sovereign edge:** `deploy/sovereign/README.md`.
- **Why N-ary / storage / honest trade-offs:** `docs/nDB-whitepaper.md`.
