# Explorer-on-production-engine: measured performance

The explorer's langgraph backend serves from the hardened `ndb-engine` in
low-memory (mmap) mode, with a precomputed tile cache (`top.json`,
`top-links.json`, `cloud.bin`, `clusters.json`) and the global current-vector
snapshot (`.vsnap`) for kNN. This note records what that combination actually
costs, measured on synthetic datasets at two scales (10× apart), so the
"is the 10 GB explorer fast?" question is answered with numbers rather than
assumptions.

## Method

`langgraph-ingest --synthetic N <db>` then `langgraph-server <db> --low-memory`,
timing cold start (first run, builds caches), warm restart (caches persisted),
per-endpoint latency, and the process memory split from `/proc/<pid>/status`.

## Results

| Metric | 300k papers (184 MB) | 3M papers (1.79 GB) | Scaling |
|---|---|---|---|
| Tile serving `/view/top?limit=500` | 5.8 ms | 5.9 ms | **flat — N-independent** |
| `/view/clusters` | 0.9 ms | 0.7 ms | flat |
| `/view/cluster/<field>` | 4.3 ms | 3.8 ms | flat |
| Engine in-RAM index heap (`/metrics`) | 76 B | 76 B | served from mmap sidecars |
| **Committed RAM** (`RssAnon`) | — | **55 MB** | **bounded** |
| Reclaimable mmap (`RssFile`) | — | 1069 MB | OS page cache, evictable |
| Cold cache-build (first run) | 3.6 s | 36 s | **linear in N** (one-time) |
| Warm restart (caches persisted) | 0.76 s | fast | caches reload, no rebuild |

## What this means

- **Tile serving is fast and N-independent.** Every per-request view
  (`top`, `clusters`, `cluster/<field>`) is served from the fixed-size
  precomputed cache, so latency is ~6 ms at 300k and at 3M alike. The
  historical "17–21 s per tile" wall was the pre-cache full-scan path; the
  tile cache removed it.
- **Committed RAM is bounded.** The engine's in-RAM indexes are 76 *bytes*
  (everything served from mmap'd sidecars), and total committed `RssAnon` is
  ~55 MB at 3M papers / 1.8 GB on disk. The eye-catching ~1.1 GB `VmRSS` is
  `RssFile` — reclaimable file-backed mmap page cache, not committed memory;
  the OS evicts it under pressure. The low-memory design holds at scale.
- **Cold cache-build is the only N-scaling cost**, and it is one-time: 3.6 s
  at 300k, 36 s at 3M, so on the order of a few minutes for a fresh 17M-paper
  (~10 GB) database. It is persisted (`top.json`/`cloud.bin`/`.vsnap`), so
  every subsequent start is fast (warm restart 0.76 s).

## The one remaining optimization

The cold cache-build runs on the **first server start** of a fresh database.
It scans every paper to build `top`/`cloud`/`.vsnap`. That work could move
into **ingest** (which already scans every paper and already writes
`clusters.json`), so even the first server start is instant on a 10 GB DB.

The obstacle is structural, not algorithmic: `langgraph-ingest` and
`langgraph-server` are separate `[[bin]]` targets that don't share code, and
the build functions (`load_or_build_top`, `build_cloud_file`, the `.vsnap`
builder + their format constants/helpers) live in the server. Doing it right
means extracting them into a shared `lib` module both binaries use — a focused
refactor, scoped for its own change rather than bolted on. Until then, the
first start on a fresh large DB pays the one-time build; serving and restarts
are already fast and bounded.
