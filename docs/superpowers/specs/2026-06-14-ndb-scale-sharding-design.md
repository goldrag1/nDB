# nDB Scale — Sharding Design (Stage 5)

**Date:** 2026-06-14
**Status:** Design for review — **needs architecture sign-off before implementation.**
**Sequence:** Sub-project 3 (final core stage) of the general-purpose DBMS product roadmap. Follows Adoptable Core (Stage 1–3) + Deploy & Operate (Stage 4), both merged.

## Finding: the perf half of Stage 5 is already done

The roadmap paired "horizontal sharding" with "10GB+ perf work." The perf gap is
**already closed**: `docs/explorer/PERFORMANCE.md` shows tile serving at **~6 ms,
N-independent** (precomputed tile cache `top.json`/`clusters.json`/`cloud.bin` +
`.vsnap` for kNN). The historical "17–21 s/tile" was the pre-cache full-scan path
and is gone. Cold cache-build is linear (~minutes at 17M papers) but one-time;
warm restart reloads caches fast. Low-RAM mmap mode (Option B) bounds RSS at 10GB.

So **Stage 5 reduces to horizontal sharding** — scaling writes/storage past one
machine. (Single-node read scaling already works via followers, Stage 4.)

## The core problem: sharding a hypergraph

nDB is **single-writer per database** (`Engine` is `&mut self` for writes, MVCC+SSI).
Scale-out = a **routing layer above N independent single-writer engines** (each an
`ndb-server` shard). The hard part unique to nDB: an **n-ary hyperedge references
entities by id** (`roles: Vec<(RoleId, EntityId)>`), and those entities may live on
different shards. A relational sharded DB only has to route rows; nDB has to decide
where an *edge spanning K shards* lives and how traversal crosses shard boundaries.

Ids are UUIDv7 (128-bit) → `hash(id)` gives even distribution for free.

## Decisions that need sign-off (each has options + a recommendation)

### D1. Shard key
- **(a) hash(entity_id)** — even load, zero config. Related entities scatter. *(recommended for v1: simplest, balanced.)*
- (b) explicit **partition key** the app sets (co-locate a tenant/graph-region on one shard). Better locality, needs a key on every entity + user discipline.
- (c) shard by **type_id** — co-locates a type; skews badly when one type dominates.

### D2. Where a hyperedge lives (the crux)
- **(a) anchor shard** — the hyperedge lives on the shard of its first role-filler. One home, simple writes; traversal from *other* members is cross-shard. *(recommended for v1.)*
- (b) **replicate to every member's shard** — local traversal from any member, but write amplification + multi-shard consistency on edge create.
- (c) **dedicated edge shard(s)** — clean separation; the edge tier becomes its own scaling bottleneck.

### D3. Cross-shard reads
- Point read by id → route to the owning shard (cheap).
- Scan / aggregate / `iter` → **scatter-gather** all shards, merge in the router.
- Vector kNN → scatter `k` to every shard, **merge top-k** by distance (correct: global top-k = merge of per-shard top-k).
- `neighbors`/traverse → resolve the entity's edges (anchor shard), then fetch role-fillers from their owning shards (fan-out by id). Bounded-depth only in v1.

### D4. Cross-shard writes
- Single-entity writes are atomic on their shard (existing MVCC) — no change.
- A hyperedge whose members span shards: v1 writes the edge **only to its anchor shard** (D2a) — no distributed transaction. The edge is durable atomically on one shard; member entities are independent rows. Accept that a member-entity delete + edge create can race across shards (document the weak guarantee; revisit with a 2-phase path later).

### D5. Routing layer shape
- A stateless **coordinator** (`ndb-router`) speaking the same `/v1` wire protocol to clients, holding a **shard map** (shard_id → server URL), routing point ops by `hash(id) % N` and scatter-gathering the rest. Clients/agents point at the router exactly like a single server — **the SDK and MCP server are unchanged.** *(recommended.)*
- Alternative: client-side routing in the SDK — pushes topology into every client, harder to evolve. Rejected.

### D6. Rebalancing / resizing
- v1: **fixed shard count** set at cluster init (no online resharding). Growing the cluster = a documented offline re-key. Online rebalancing (consistent hashing + range moves) is a large follow-up. *(defer.)*

## Proposed architecture (pending D1–D6 sign-off)

```
clients / agents ──/v1──► ndb-router (stateless coordinator, shard map)
                              ├─ point op:  hash(id)%N → shard k
                              ├─ scan/agg:  scatter all shards → merge
                              └─ kNN:        scatter k → merge top-k
                          shard 0..N-1 = ndb-server (single-writer engine each,
                                          + Stage-4 followers per shard for read HA)
```

The router is a new crate `ndb-router` reusing `ndb-client-rust` to talk to shards
and the existing `/v1` request/response types. Shards are unmodified `ndb-server`s.
Composes with Stage 4: each shard can itself be a leader + followers.

## Decomposition into buildable increments

1. **Shard map + router skeleton** — `ndb-router` serving `/v1/health` + a static
   shard map (config file / flags); no routing yet. Tiny, establishes the crate.
2. **Point-op routing** — `commit`(single entity) / `read`/`lookup` routed by
   `hash(id)%N`. Integration test: 2 shards, write→correct shard, read routes back.
3. **Scatter-gather reads** — `iter`/`query`/`property_*` fan out + merge. Test
   counts across shards.
4. **Hyperedge anchor writes + cross-shard traverse** — D2a placement; `neighbors`
   fans out to member shards. The correctness-critical increment; heavy testing.
5. **Vector kNN merge** — scatter-k, merge top-k; test global ranking == single-node.
6. **Compose/Helm topology** — a sharded cluster (router + N shard StatefulSets).

Each increment is independently testable + shippable, same as Stages 1–4.

## Non-goals (this stage)
Online resharding / rebalancing (D6), distributed ACID transactions across shards
(D4), cross-shard secondary-index consistency beyond scatter-merge, automatic
shard failover (composes with the separate Raft sub-project per shard).

## Why this needs sign-off before code
Sharding is architecturally invasive and the decisions above (especially D2 edge
placement) are one-way doors — they shape the wire semantics, the test matrix, and
what guarantees we can later make. Per the project's "architecture decisions need
confirmation" rule, D1–D6 should be agreed before increment 1. Implementation is a
**multi-session effort**; increment 1–2 are a reasonable first focused session.
