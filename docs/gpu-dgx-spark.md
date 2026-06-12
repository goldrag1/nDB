# nDB on the GPU: the Arrow on-ramp & NVIDIA DGX Spark (GB10)

> How nDB feeds a GPU, and what's specific about running that pipeline on
> NVIDIA's DGX Spark — the GB10 Grace Blackwell desktop AI machine.

## The model: nDB serves, the GPU computes

nDB is a **CPU storage + index engine** by design — there is no CUDA or SIMD in
the source; the HNSW vector index is pure-Rust `instant-distance`. GPUs don't run
nDB; they consume data nDB serves, across **Apache Arrow** (`crates/ndb-arrow`),
the zero-copy interchange the whole GPU-data stack reads.

```
nDB (CPU: store · MVCC · hyperedges · ANN candidate filter)
        │  Arrow IPC / RecordBatch  (typed columns, 64-byte aligned buffers)
        ▼
GPU framework (Blackwell: batched ANN re-rank · GNN train/infer · embedding gen)
```

## What DGX Spark changes

[NVIDIA DGX Spark](https://www.nvidia.com/en-us/products/workstations/dgx-spark/)
is built on the **GB10 Grace Blackwell Superchip**. The two facts that matter for
nDB:

| Property | Value | Why it matters to nDB |
|---|---|---|
| **CPU** | 20-core ARM (10× Cortex-X925 + 10× Cortex-A725), **aarch64** | nDB must *build and run on ARM*. Verified — see below. |
| **Memory** | **128 GB LPDDR5X unified**, coherent Grace↔Blackwell over NVLink-C2C (~273 GB/s) | Arrow buffers nDB writes in host memory are **directly addressable by the GPU — no host→device copy**. |
| GPU | Blackwell, 48 SMs / 6,144 CUDA cores, compute capability **sm_121**, NVFP4 | runs the heavy math (ANN re-rank, GNN, embedding gen) |
| Platform | Ubuntu 24.04, **CUDA 13.0** | standard RAPIDS / cuVS / cuGraph stack |

The unified memory is the headline. On a discrete-GPU box, the Arrow→GPU path
costs a PCIe `cudaMemcpy`. On Spark, Grace and Blackwell share one physical pool,
so a RAPIDS reader (`cudf.from_arrow`, `cuvs`) can adopt the Arrow buffers in
place. nDB's job is just to hand over **well-formed, contiguously-laid-out Arrow**
— which is exactly what the helpers below produce.

### aarch64 portability — verified

The entire GPU-relevant data path compiles for the Spark target:

```bash
rustup target add aarch64-unknown-linux-gnu
cargo check --target aarch64-unknown-linux-gnu \
    -p ndb-engine -p ndb-index-vector-hnsw -p ndb-arrow
# Finished `dev` profile [...]
```

This works because the hot paths are pure-safe-Rust with no x86 intrinsics and no
C++ toolchain (the HNSW index is explicitly "no SIMD intrinsics"). Arrow-rs is
cross-platform. Nothing in the store/index/export path assumes x86.

## The Rust surface (`ndb-arrow`)

| Function | Output | GPU consumer |
|---|---|---|
| `records_to_batches(records, batch_rows)` | `Vec<RecordBatch>`, one schema | stream a dataset larger than RAM (B1) |
| `records_to_ipc_stream_chunked(records, batch_rows)` | multi-batch Arrow IPC bytes | pyarrow / cuDF / Polars |
| `vector_column_batch(records, type_id, property_id)` | `primary_id` + `embedding: FixedSizeList<Float32, dim>` | **cuVS** exact ANN re-rank (B2) |
| `hyperedge_edge_index(records)` | `(hyperedge_id, role_id, participant_id, participant_kind)` incidence list | **cuGraph / DGL / PyG** hypergraph GNN (B3) |

`Value::Decimal` now maps to Arrow's native **`Decimal128(38, scale)`** — lossless
(B4), so money/measurement columns survive the trip to the GPU exactly.

### Over the wire (no Rust required)

The same helpers are exposed by both servers, gated by the `read` capability, so
a Python client can pull Arrow directly:

```bash
# HTTP: raw application/vnd.apache.arrow.stream bytes
curl -s 'http://localhost:8080/arrow/vectors?type_id=10&property_id=100' -o emb.arrows
curl -s 'http://localhost:8080/arrow/edge_index' -o edges.arrows
curl -s 'http://localhost:8080/arrow/export?batch_rows=65536' -o all.arrows
```

```python
import pyarrow as pa, requests
buf = requests.get("http://localhost:8080/arrow/vectors",
                   params={"type_id": 10, "property_id": 100}).content
batch = pa.ipc.open_stream(buf).read_all()   # primary_id | embedding
```

Over MCP the same three tools (`ndb.arrow_export`, `ndb.arrow_vectors`,
`ndb.arrow_edge_index`) return the IPC stream base64-encoded in the JSON-RPC
result, so an AI agent can request a GPU-ready export itself.

## Application 1 — RAG / agent memory: nDB filters, GPU re-ranks

nDB's HNSW returns coarse candidates on the CPU; the GPU does exact distance over
their vectors. `vector_column_batch` hands those vectors over as a dense matrix.

```python
import pyarrow as pa
from pyarrow import ipc
import cudf            # RAPIDS
from cuvs.neighbors import brute_force   # exact GPU re-rank

# bytes from ndb_arrow::vector_column_batch(...) via the server / FFI
reader = ipc.open_stream(arrow_ipc_bytes)
batch = reader.read_all()                # primary_id | embedding (FixedSizeList)

# On DGX Spark's unified memory this adopts the buffers in place — no copy.
gdf = cudf.DataFrame.from_arrow(batch)
emb = gdf["embedding"].list.leaves.values.reshape(len(gdf), -1)  # [n, dim] device

# exact top-k on the GPU over the candidate set nDB pre-filtered
distances, indices = brute_force.search(brute_force.build(emb), query, k=10)
```

## Application 2 — hypergraph GNN: one record → one edge index

nDB stores an n-ary relationship as a single hyperedge record;
`hyperedge_edge_index` flattens every hyperedge into the bipartite
`(edge, participant)` incidence list a GNN stack consumes — **no junction-table
re-join**, the structural win from the storage model carried to the GPU.

```python
import cudf, cugraph
edges = cudf.DataFrame.from_arrow(edge_index_batch)   # hyperedge_id | role_id | participant_id | kind
G = cugraph.Graph()
G.from_cudf_edgelist(edges, source="hyperedge_id", destination="participant_id")
# → feed into a hypergraph GNN (DGL / PyG) for link prediction, node classification
```

## Application 3 — scientific / vector analytics

Store 768–1536-d embeddings, biological sequences, or blobs; take a bitemporal
`as_of` slice via the engine; export it chunked to cuDF / Polars-GPU for batched
similarity, clustering, or embedding generation on Blackwell.

## Honest limits (today)

- **The Arrow buffers are produced in host memory.** On Spark that's already
  GPU-addressable (unified memory), so it's effectively zero-copy. On a
  discrete-GPU host it still costs one PCIe copy — that's expected.
- **No GPUDirect Storage path** (disk → GPU bypassing host) — out of near-term
  scope; tracked as B4-later in `roadmap-agent-gpu.md`.
- **`vector_column_batch` requires uniform vector dimension** per call (it errors
  on a mismatch) — correct for an embedding column, by construction.
- nDB itself does no GPU compute and is not planned to; the division of labour
  (CPU storage/filter, GPU math) is deliberate.
- **On-device GPU compute is unverified.** The Arrow data contract is
  machine-verified (every export is read back through the standard Arrow reader in
  tests) and the path is aarch64-clean (CI-guarded), but actually running cuVS /
  cuGraph kernels on a physical DGX Spark needs the hardware — that's the one open
  item.

## Sources

- [NVIDIA DGX Spark product page](https://www.nvidia.com/en-us/products/workstations/dgx-spark/)
- [DGX Spark Hardware Overview (NVIDIA docs)](https://docs.nvidia.com/dgx/dgx-spark/hardware.html)
- [GB10 / unified memory / sm_121 deep-dive (Kubesimplify)](https://blog.kubesimplify.com/day-3-the-dgx-spark-unpacked-gb10-unified-memory-sm-121-and-the-one-reason-this-hardware-exists)
- [Arm: quantized LLM on DGX Spark (GB10 setup)](https://learn.arm.com/learning-paths/laptops-and-desktops/dgx_spark_llamacpp/1a_gb10_setup/)
