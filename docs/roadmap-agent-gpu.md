# Roadmap: AI-Agent-Native nDB & the GPU On-Ramp

> Status snapshot as of this branch. Grounded in the actual crates
> (`ndb-mcp-server`, `ndb-arrow`, `ndb-index-vector-hnsw`, `ndb-engine`), not
> aspiration. "Shipped" means code exists and is tested; "Building" means started
> in this branch; "Planned" means designed, not yet coded.

## Part A — AI-agent native

nDB already speaks **Model Context Protocol** (`crates/ndb-mcp-server`): JSON-RPC 2.0
over stdio, embedded engine, per-tool **capability gating (ReBAC)** + **audit log**.
That's the protocol Claude / coding agents already speak. The v1 gap was that the
tool surface didn't expose nDB's *distinctive* model (hyperedges, time-travel) —
**now closed (A1–A4)**: agents can create and traverse n-ary relationships, do
time-travel reads, paginate, read typed tool schemas, and pull schema/stats as MCP
resources.

### A1. Hyperedge agent tools — **✅ Shipped (this branch)**

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

### A2. Richer tool descriptions & JSON schemas — **✅ Shipped (this branch)**

v1 `tools/list` returns name + one-line description only. Agents call tools more
reliably with `inputSchema` (JSON Schema) per tool. Add `inputSchema` to every entry
in `tool_list()`.

### A3. Streaming / pagination — **✅ Shipped (this branch)**

`ndb.iter` is hard-capped at 1000 records, no cursor. Add `{cursor, limit}` →
`{records, next_cursor}` so an agent can walk an arbitrarily large DB. Pairs with the
Arrow chunking work in Part B.

### A4. MCP resources & prompts — **✅ Shipped (this branch)**

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

**Target hardware includes NVIDIA DGX Spark (GB10 Grace Blackwell).** Two facts
shaped the work: it's **aarch64** (so the path must build on ARM — verified, see
B0) and it has **128 GB unified Grace↔Blackwell memory** (so the Arrow buffers
nDB writes are GPU-addressable with no host→device copy). Full details and Python
recipes in [`gpu-dgx-spark.md`](./gpu-dgx-spark.md).

### B0. aarch64 / DGX Spark portability — **✅ Verified (this branch)**

`cargo check --target aarch64-unknown-linux-gnu` passes for `ndb-engine`,
`ndb-index-vector-hnsw`, and `ndb-arrow` — the whole store/index/export path runs
on Grace ARM. Works because the hot paths are pure-safe-Rust with no x86
intrinsics and no C++ toolchain.

### B1. Chunked / streaming Arrow export — **✅ Shipped (this branch)**

`ndb-arrow` gained `records_to_batches(records, batch_rows)` and
`records_to_ipc_stream_chunked(...)`: the column set is discovered once across all
records, then data rows are windowed into many `RecordBatch`es under one schema —
streamable to a GPU consumer for datasets larger than host RAM.

### B2. Vector column → GPU ANN handoff — **✅ Shipped (this branch)**

`vector_column_batch(records, type_id, property_id)` emits a dense
`primary_id + embedding: FixedSizeList<Float32, dim>` batch — the `[n, dim]`
layout cuVS wants. nDB HNSW returns coarse candidates on CPU; the GPU does exact
re-rank over their vectors. Python recipe in `gpu-dgx-spark.md`.

### B3. Hyperedge → GNN edge-index export — **✅ Shipped (this branch)**

`hyperedge_edge_index(records)` flattens every hyperedge into the bipartite
`(hyperedge_id, role_id, participant_id, participant_kind)` incidence list cuGraph
/ DGL / PyG consume — no junction-table re-join, the structural advantage carried
to the GPU.

### B4. Lossless decimal — **✅ Shipped** · GPUDirect — **Later**

`Value::Decimal` now maps to Arrow `Decimal128(38, scale)` — exact, not the old
lossy `Float64`; mixed-scale columns widen to the max scale and rescale each value
losslessly. GPUDirect Storage (disk → GPU, bypassing host RAM) remains a much
larger effort, explicitly out of near-term scope.

## Target applications where a GPU uses nDB as the data layer

| Application | nDB (CPU) | GPU (Blackwell) | Bridge |
|---|---|---|---|
| RAG / agent memory at scale | embeddings + entities, HNSW candidate retrieval | exact re-rank over millions, run the LLM | Arrow → cuVS |
| GNN on hypergraphs | nodes + n-ary hyperedges, serve subgraph | train/infer the graph net | Arrow → cuGraph/PyG |
| Scientific & vector analytics | store 768–1536-d vectors, sequences, blobs; `as_of` slices | batched similarity, clustering, embedding gen | Arrow → cuDF/Polars-GPU |
| High-QPS vector ANN | source-of-truth + incremental index | GPU ANN where CPU-HNSW saturates | Arrow → GPU index lib |

## Sequence

**Done on this branch:** A1, A2, A3, A4 (agent surface) and B0, B1, B2, B3, B4
(GPU on-ramp + aarch64/DGX-Spark portability). All compile, are unit-tested, and
are clippy-clean in their crates.

**Remaining / future:**
1. Wire the new `ndb-arrow` GPU helpers through `ndb-server` / `ndb-mcp-server`
   so an agent can request an Arrow export over the wire (currently a library
   API).
2. End-to-end validation on real DGX Spark hardware (cuVS re-rank, cuGraph GNN)
   — code is aarch64-clean but unverified on-device.
3. GPUDirect Storage (disk → GPU) — large effort, deferred.
