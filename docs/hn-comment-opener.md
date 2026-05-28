# HN top-comment opener (paste as first reply when you submit)

> Self-comment to front-load what the bench table doesn't make screaming
> obvious.

The honest weakness in the writeup is the **two-pattern join** row: nDB
runs ~80× slower than Postgres at N=100k. That's the row that will get
called out first in this thread, and rightly so. Here's the why, and
the fix path:

**Why nDB loses on two-pattern joins right now.** The executor
materialises bindings row-by-row. The query `match customer(region: X)
as ?c sales(buyer: ?c)` resolves customer (~49 rows from the property
B-tree) then walks each one's adjacency for sales (~10 each) =
~490 intermediate binding rows. Each row goes through
`Bindings::clone() + snapshot_read()` on every join step. That's
~91 μs per intermediate row, dominated by the clone — totally fine
for adjacency walks of degree 5-20, totally not fine for any join
shape that expands to thousands of rows. Postgres' HashJoin builds a
hash on the 49 customers (~13 ns per slot) then probes the 45k sales
rows at ~13 ns each — 0.6 ms total, ~80× faster.

The architectures are working as designed. nDB optimises for
"adjacency walk per anchor entity", which always wins when the fanout
is bounded by the graph's structure. SQL optimises for "build a hash,
probe a hash", which always wins when the join cardinality is bounded
by table size and the join key is amenable to hashing. The realworld
microbench catches a join shape that's in PG's wheelhouse.

**Fix path.** The executor is going to be rewritten as a streaming
iterator pipeline. Each pattern becomes an Iterator<Bindings>; joins
become a streaming hash-join (build on the smaller side, probe on
the larger). Same query language, same wire format, just no more
row-by-row clone in the hot path. That's v2 work, called out
explicitly in the bench notes as the reason this row is what it is.
The streaming executor also lands `LIMIT` pushdown to the iterators,
which removes another class of materialise-everything-then-truncate
costs.

**Where it doesn't matter.** The biology bench at N=250k with hub-
routed edges (linked from the repo's bench dashboard) shows nDB
beating Postgres ~1.8× on the multi-hop traversal that motivates
the engine — that's a join shape too, but it's the kind where each
anchor's fanout stays bounded by the graph and the recursion depth
matters more than the cardinality. The realworld microbench and the
biology macro-bench together cover the workload-shape spectrum
fairly: nDB-favourable on the right side, PG-favourable on the left.

**Where nDB structurally wins regardless of executor.** Recursive
closure (`contains+`) — 26× faster on this microbench, and the gap
widens with depth + fanout, because PG materialises a workset table
per recursion step while nDB walks the in-memory adjacency index
directly. That's the kind of workload that motivates the data model;
the executor rewrite doesn't change it.

Happy to dig into any specific workload shape — paste the SQL you'd
write and I'll show what the same query looks like in nDB plus the
planner output (the `EXPLAIN` route + the "Show plan" toggle in the
playground are there exactly for this).

— author
