# nDB — a hypergraph engine for facts with more than two endpoints

Most events in the world have more than two participants, and most databases
require you to break those events apart to store them. nDB doesn't. It is a
small Rust storage engine where the primitive is a **hyperedge with N
named role-players plus properties**, plus a Datalog-influenced query
language that drops onto it. A single binary, a single file format, an HTTP
+ JSON wire surface, and a 488-test suite that goes green in 0.2 s.

This is a Show-HN draft. The code is at <https://github.com/goldrag1/nDB>
on `main` at tag `v2.3.0`. Five interactive demos run live at
<https://ndb.nextstar-erp.com>. The numbers below are reproducible — every
JSON file is committed.

## The N-ary problem in one example

Consider yucca pollination. The yucca plant is fertilised by a single moth
species (Tegeticula yuccasella) which lays its eggs inside the yucca's
ovary at the same visit. Six dimensions matter for the biologist:
**plant**, **pollinator**, the **region** in which the event occurs, the
**season window** the interaction is observed in, the **interaction type**
("obligate mutualism"), and the **citation** that documents it. In any
real biological knowledge graph you also want the *time the assertion was
recorded* — i.e. the engine's transaction id — for reproducibility.

In a relational schema you flatten this into a `pollination_event` table
plus three foreign-key tables plus a `references` junction table plus an
audit log. To get back the original event you write a four-way JOIN. To
ask "give me all 4+ arity interactions involving any plant in Vietnam"
you write a CTE. In Neo4j you reify the event as a node and hang four
binary edges off it; the atomicity is gone.

In nDB it's one record:

```ndb
match pollination(
    plant:      ?p,
    pollinator: ?m,
    region:     ?r,
    season:     ?s,
    type:       "obligate mutualism"
)
return ?p.common_name, ?m.scientific_name, ?r.name, ?s
```

That `pollination(...)` is not a row + 5 junction rows. It is one
`HyperEdgeRecord` with `arity = 5` whose `roles` field is a
`Vec<(RoleId, EntityId)>`. The arity field is `u32`, so KRAS's
`protein_atoms` hyperedge with 1518 role-fillers is the same primitive
as a 2-ary `predation(predator: ?x, prey: ?y)`. Same shape, same code
path, same byte layout.

## The data model

There are three record kinds on disk: **Entity** (an addressable thing
with properties), **HyperEdge** (a typed connection with named roles
and optional properties), and **Tombstone** (a supersedure marker for
MVCC). Every record carries an explicit `tx_id_assert` (when it was
written) and `tx_id_supersede` (when it was retracted, or
`TxId::ACTIVE` if live). Names — type names, role names, property
names — are dictionary-encoded as `u32` ids, with separate
`TypeName`, `RoleName`, `PropertyKey` records that the engine resolves
on every query. Names can be added live; ids never collide.

Storage is an append-only LSM: a write goes to the in-memory memtable
+ a `.ndblog` WAL, a `flush()` rolls the memtable to an immutable
`.sst` file under a versioned MANIFEST. Compaction merges SSTables
with cross-bucket tombstone handling. Six mandatory in-memory indexes
are rebuilt from the snapshot on open: the lookup-key index (unique
external keys → uuid), the property B-tree
`(type_id, property_id, value) → entity_ids`, the adjacency index
`entity → hyperedge[]`, the type-cluster index, the
`hyperedge_type → hyperedges[]` cluster, and the vector index
(brute-force in core; HNSW via `ndb-index-vector-hnsw`).

Schemas live inside the data as **metadata hyperedges**: a
`ValidationConstraint` hyperedge whose role-fillers point at the type
and property it constrains, with property fields naming the constraint
kind. The validation engine reads these on commit. There is no
separate schema artefact. To extend the model with a new dimension you
add a new role to the hyperedge definition; existing records continue
to validate. To change the constraint shape you write a new
`ValidationConstraint` and let MVCC handle the old one.

## The query language, in one paragraph and one query

A query is one or more pattern atoms separated by whitespace,
optionally followed by `where`, `order by`, `limit`, write clauses
(`create` / `delete` / `set` / `merge`), and an optional `as of` time
travel selector. Each atom is `type(role: term, role: term, ...) as ?var`.
Variables shared across atoms are the join keys. Recursion is a `+`,
`*`, `?`, or `{n,m}` suffix on the type name. Aggregates (`count`,
`sum`, `avg`, `min`, `max`) auto-group on every non-aggregate return
item. The whole grammar is in
[`docs/superpowers/specs/2026-05-27-query-language.md`](superpowers/specs/2026-05-27-query-language.md).

```ndb
# Three-pattern join + property projection + recursive walk:
# find plants whose pollinators are found in any region inside
# Vietnam (transitive containment, depth ≤ 4).

match
  pollination(plant: ?p, pollinator: ?m)
  occurs_in(species: ?m, region: ?r)
  contains+(parent: "VN", child: ?r) {1,4}
return
  ?p.common_name, ?m.scientific_name, ?r.name
order by ?p.common_name asc
limit 100
```

Try this live in the playground at
<https://ndb.nextstar-erp.com/query-language.html>. The "Show plan"
button calls `POST /query/explain` and renders the planner's chosen
seed atom + per-step cardinality estimates — no execution, no commit,
useful for understanding why one phrasing of a query beats another
even when both return the same rows.

## What the numbers actually look like

The realworld micro-benchmark loads 50,000 entities + 50,000
hyperedges + a region-containment chain into a fresh nDB and into a
parallel Postgres with the natural junction-table schema (indexed on
both join keys, `ANALYZE` before timing). Both run single-threaded
release builds on the same Linux host. The full report with notes is
at
[`docs/benchmarks/realworld-2026-05-28.md`](benchmarks/realworld-2026-05-28.md);
the headline:

| workload                              | nDB p50 | nDB ops/s   | PG p50  | PG ops/s |
|---------------------------------------|--------:|------------:|--------:|---------:|
| Full snapshot scan (100k records)     | 21 ms   | 45/s        | 92 ms   | 12/s     |
| Random point lookup by UUID           | <1 μs   | 953,000/s   | 19 μs   | 38,000/s |
| Indexed property lookup               | <1 μs   | 1,300,000/s | 77 μs   | 10,000/s |
| Single-pattern query                  | 29 μs   | 33,000/s    | 54 μs   | 18,000/s |
| Two-pattern join                      | 46 ms   | 21/s        | 0.6 ms  | 1,600/s  |
| Recursive walk, depth 3               | 37 μs   | 26,000/s    | 968 μs  | 1,000/s  |
| count() over a type (49k rows)        | 46 ms   | 21/s        | 1.5 ms  | 640/s    |
| Sequential single-record commits      | 339 μs  | 2,800/s     | 265 μs  | 3,200/s  |

nDB wins ~125× on indexed-property lookup, ~26× on recursive closure,
and ~50× on point lookup against memtable-resident data. It loses ~80×
on the two-pattern hash join and ~30× on `count()` — the executor
currently materialises bindings row-by-row, which is the right shape
for adjacency walks (where it always wins) and the wrong shape for
equi-joins (where Postgres' hash-build-probe wins). Streaming the
executor is v2 work and explicitly called out as such in the benchmark
notes.

The biology bench at the project's dashboard (separate workspace at
`/home/long/long/rust/`) runs the same data shape at N up to 250k with
hub-routed edges that concentrate fanout 20× on every 20th protein
slot. There nDB pulls ahead of Postgres from N≈10k on multi-hop
traversals — the architecture's home turf — and the gap widens to
~1.8× at N=50k. The two benches are consistent: nDB favours
adjacency-walk + recursive-closure workloads; Postgres favours
hash-equi-join workloads. At small N both engines are within an order
of magnitude of each other on every workload.

## Time travel

Every read accepts a snapshot. The wire form is `?snapshot=<tx_id>`
on any GET; the query form is `as of tx_id <N>` prefixed to a
`match`:

```ndb
as of tx_id 42
match species(life_form: "plant") as ?s return ?s.common_name
```

The engine resolves visibility per record via the MVCC
`(tx_id_assert, tx_id_supersede)` pair. A record committed at tx 50
is invisible at tx 42; a record retracted (tombstoned) at tx 60 is
visible at tx 50 and invisible at tx 70. Compaction can drop a
retracted record only after every snapshot before its supersede tx
is closed. There is no "database vacuum" knob — the engine knows the
oldest active snapshot it owes a read to.

## What this isn't

This is not a SQL replacement. There are no subqueries, no CTEs, no
window functions, no `having`. Aggregates beyond
`count|sum|avg|min|max` are not implemented; `where` evaluates
comparisons + boolean ops but not property access (you bind the
property in the pattern instead). There is no production-grade
ReBAC — the principal allowlist is in-memory JSON. There is no
distribution; nDB is single-node, single-writer. The bench numbers
above are not concurrent.

The five demos at `ndb.nextstar-erp.com` (AlphaFold protein
structures, Exoplanet 4-arity discovery records, Seismic events,
Chemistry reaction networks, Biodiversity food webs) run with
`Server::with_read_only(true)` — write clauses in the query language
return `403 read_only`. The same binary running locally without that
flag executes every clause normally.

## Where the code is

```bash
git clone https://github.com/goldrag1/nDB
cd nDB
cargo test                                  # 488 tests in 0.2 s
cargo run --release --example basic         # 50-line tour
bash tools/bench/run_realworld.sh           # reproduce the table above
```

The 8-crate workspace is laid out so each layer's surface is small:

- `ndb-engine` — storage core (records, WAL, SSTable, MVCC, indexes,
  validation, encryption primitives)
- `ndb-query` — lexer + parser + name resolver + run wrapper
- `ndb-server` — hand-rolled HTTP/1.1, no async runtime, rustls TLS
- `ndb-cli` — `ndb` binary, thin frontend over `ndb-client-rust`
- `ndb-mcp-server` — stdio JSON-RPC MCP bridge for LLM agents
- `ndb-slicer` / `ndb-renderer` / `ndb-arrow` — companion compute +
  output crates
- `clients/python/ndb_client` — pure-stdlib Python HTTP client

The authoritative design spec is
[`docs/superpowers/specs/2026-05-27-nDB-hypergraph-design.md`](superpowers/specs/2026-05-27-nDB-hypergraph-design.md);
locked decisions are listed by module preamble (no re-litigating
WAL format / sort key / single-writer concurrency / wire format
without surfacing the existing rationale). v1 + v2 + v3 release
notes are in commit history.

Feedback welcome on whether the two-pattern join cost is a deal-
breaker for your workload, whether the query language is missing
something obvious, and whether the planner `EXPLAIN` output reads
clearly. Both ergonomic and structural critique gets faster
turnaround than feature requests — the v3 surface stays small on
purpose.
