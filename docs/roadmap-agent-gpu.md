# Roadmap: AI-Agent-Native nDB & the GPU On-Ramp

> Status snapshot as of this branch. Grounded in the actual crates
> (`ndb-mcp-server`, `ndb-arrow`, `ndb-index-vector-hnsw`, `ndb-engine`), not
> aspiration. "Shipped" means code exists and is tested; "Building" means started
> in this branch; "Planned" means designed, not yet coded.

## Part A — AI-agent native

nDB already speaks **Model Context Protocol** (`crates/ndb-mcp-server`): JSON-RPC 2.0
over stdio, embedded engine, per-tool **capability gating (ReBAC)** + **audit log**.
That's the protocol Claude / coding agents already speak. The gap is not the
transport — it's that the v1 tool surface doesn't expose nDB's *distinctive* model
(hyperedges, time-travel). An agent can read/write flat entities but cannot create
or traverse the n-ary relationships that make nDB nDB.

### A1. Hyperedge agent tools — **Building (this branch)**

The differentiator from the storage comparison: a relationship of any arity is one
record. Exposed as MCP tools so an agent can model n-ary knowledge directly.

| Tool | Shape | Capability |
|---|---|---|
| `ndb.commit_hyperedge` | `{type_id, roles:[{role_id, entity_id}], hyperedge_roles?:[{role_id, hyperedge_id}], properties?}` → `{tx_id, hyperedge_id}` | `commit` |
| `ndb.neighbors` | `{uuid, limit?}` → incident hyperedges + connected entities (1-hop traversal) | `read` |
| `ndb.read_as_of` | `{uuid, as_of_tx?, as_of_timestamp_us?}` → time-travel read | `read` |

These three close the create / traverse / time-travel gap. Built on existing engine
primitives: `WriteTxn::put_hyperedge`, `Engine::hyperedges_for_entity_capped`,
`Engine::tx_at_or_before` + `snapshot_read`.

### A2. Richer tool descriptions & JSON schemas — **Planned**

v1 `tools/list` returns name + one-line description only. Agents call tools more
reliably with `inputSchema` (JSON Schema) per tool. Add `inputSchema` to every entry
in `tool_list()`.

### A3. Streaming / pagination — **Planned**

`ndb.iter` is hard-capped at 1000 records, no cursor. Add `{cursor, limit}` →
`{records, next_cursor}` so an agent can walk an arbitrarily large DB. Pairs with the
Arrow chunking work in Part B.

### A4. MCP resources & prompts — **Planned**

v1 ships `tools/*` only. Resources (read-only context blobs: schema/dictionary
snapshots) and prompts (query templates) would let an agent discover the schema
without trial-and-error tool calls.

## Part B — GPU on-ramp (Blackwell & friends)

nDB is a **CPU storage + index engine** by design — zero CUDA/SIMD in the source
(the HNSW index is pure-Rust `instant-distance`, explicitly "no SIMD intrinsics").
GPUs don't run nDB; they consume data nDB serves. The bridge is **Apache Arrow**
(`crates/ndb-arrow`), the zero-copy interchange the entire GPU-data stack reads
(RAPIDS cuDF/cuVS/cuGraph, Polars-GPU, DGL/PyG).

```
nDB (CPU: store · MVCC · hyperedges · ANN candidate filter)
        │  Arrow IPC (zero-copy, typed columns)
        ▼
GPU framework (Blackwell: batched ANN re-rank · GNN train/infer · embedding gen)
```

### B1. Chunked / streaming Arrow export — **Planned (next after A1)**

Today `ndb-arrow` is single-batch, in-memory ("for very large databases callers
should chunk themselves"). Add `snapshot_iter_chunked(by: (record_kind, type_id),
batch_rows)` → an iterator of `RecordBatch` so a GPU consumer can stream a dataset
larger than host RAM. This is the unlock for feeding Blackwell at scale.

### B2. Vector column → GPU ANN handoff — **Planned**

`Value::Vector` already maps to Arrow `List<Float32>`. Document + provide a thin
Python recipe: nDB HNSW returns top-N candidates on CPU → hand the candidate vectors
(as one Arrow batch) to cuVS for exact GPU re-rank. nDB stays the source of truth and
the coarse filter; the GPU does the heavy distance math.

### B3. Hyperedge → GNN edge-index export — **Planned**

Hyperedges already flatten to an Arrow `roles` column
(`List<Struct{role_id, entity_id}>`). Add a helper that emits a bipartite
(hyperedge, participant) edge index directly, so cuGraph/PyG can build a hypergraph
GNN without a re-join — the structural advantage from the storage comparison carried
all the way to the GPU.

### B4. Lossless decimal & GPUDirect — **Later**

`Value::Decimal` currently widens to `Float64` (lossy). Move to Arrow's native decimal
type. GPUDirect Storage (disk → GPU, bypassing host RAM) is a much larger effort and
explicitly out of near-term scope.

## Target applications where a GPU uses nDB as the data layer

| Application | nDB (CPU) | GPU (Blackwell) | Bridge |
|---|---|---|---|
| RAG / agent memory at scale | embeddings + entities, HNSW candidate retrieval | exact re-rank over millions, run the LLM | Arrow → cuVS |
| GNN on hypergraphs | nodes + n-ary hyperedges, serve subgraph | train/infer the graph net | Arrow → cuGraph/PyG |
| Scientific & vector analytics | store 768–1536-d vectors, sequences, blobs; `as_of` slices | batched similarity, clustering, embedding gen | Arrow → cuDF/Polars-GPU |
| High-QPS vector ANN | source-of-truth + incremental index | GPU ANN where CPU-HNSW saturates | Arrow → GPU index lib |

## Sequence

1. **A1 — hyperedge agent tools** ← in progress on this branch.
2. A2 — per-tool JSON schemas (cheap, high agent-reliability payoff).
3. B1 — chunked Arrow export (unlocks every GPU path).
4. A3 / B2 / B3 — streaming + GPU handoff recipes.
5. A4 / B4 — resources/prompts, lossless decimal, GPUDirect.
