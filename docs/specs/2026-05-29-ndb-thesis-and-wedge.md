# nDB — Thesis, Wedge, and the OpenAlex Proof

Status: VISION / STRATEGY (not an implementation plan)
Date: 2026-05-29
Author: office-hours session (goldrag1)
Scope: positioning + roadmap gate. Engine execution is a separate session.

## The thesis (state it precisely — the sloppy version gets dismissed)

Relational databases are a 1970s compromise. Codd normalized real-world objects into
flat tables because disk and compute were precious: you shatter an object across rows and
rebuild it at query time with JOINs. That was never how the object *is* — it's how we could
afford to store it. A paper is not `works` + `authorships` + `referenced_works`; it is one
thing with authors, citations, a position in idea-space, and a life over time.

nDB's bet: **store the object n-dimensionally — don't slice it into 2D table projections —
then project it into 3D + time + interaction so a human brain can walk through it.**

Two clauses, never one:
1. **Storage:** one addressable object with N-ary edges + vector + MVCC. Not decomposed.
2. **Projection:** the explorer's 5D (x/y/z + year-as-MVCC + semantic-layout) is a *view for
   the eye*, NOT a claim that data lives in 5 spatial dimensions.

Stated sloppily ("we store in 5 dimensions") an engineer dismisses it in one sentence.
Stated as above it is defensible — and the supporting narrative is real: AI already "thinks"
in high-dimensional embeddings; humans are still reading 2D tables. The tool that lets a human
*perceive* higher-dimensional structure is a real capability, and the brain adapts to the
dimensionality it's given (2D/3D today → more).

"n" = nature (real-world objects are n-dimensional) and the math sense (any/unlimited number).

## What nDB IS

A platform. **The OpenAlex visualization is ONE application — the first proof, not the thesis.**
Applications of an n-dimensional store are to be discovered (this is the beginning, the way
Postgres/Oracle were bets whose full application surface emerged later). OpenAlex is chosen as
proof #1 because it is the largest *legal* (CC0) high-dimensional graph available.

## Premises (all agreed this session)

1. **Wedge = the exploration/visualization LAYER, not system-of-record replacement.** Don't rip
   out OpenAlex's Postgres+Elasticsearch. Be the n-dimensional *lens* over knowledge graphs.
   This plays to nDB's real strengths (graph traversal, vector kNN, MVCC time-travel — the three
   things relational stores do badly) and sidesteps the near-unwinnable "replace our prod DB" pitch.
2. **Engine-first is the non-negotiable gate.** The public explorer is worthless if the server
   can't serve. Today it dies at ~0.04% of OpenAlex: `view/top` took **46s at 10GB** vs the client's
   8s tile timeout; kNN is a 15s full-embedding scan. Milestone 1 is NOT prettier data — it's an
   engine that serves a 10GB+ graph at <1s/tile.
3. **Thesis stated precisely** ("store n-D, project to 3D+time+interaction"), never as "store in
   N spatial dimensions."
4. **Free public explorer = attention + proof; revenue from a NAMED adjacent, not the free thing.**
   "Attention always monetizes eventually" is survivorship bias — the science-viz graveyard
   (Connected Papers, ResearchRabbit, Litmaps, VOSviewer) got attention and still struggle, because
   their users are researchers (no budget) on free data (no toll booth). Attention monetizes when you
   control a *mandatory adjacent*. Candidate toll booths, decide later:
   - (a) **Engine licensing** — orgs with proprietary high-dimensional data who want this layer.
   - (b) **Pro/enterprise analytics** — R&D / IP / competitive-intel / funders (they DO have budget)
     on top of the free public explorer.
   - (c) **Embedded-explorer** — be the default "explore" layer that data publishers embed.
   The free children-and-students explorer is the funnel and the proof, not the revenue.

## Comps, history, and the hard rules (researched 2026-05-29)

### Why the science-viz app layer doesn't monetize (verified)
- **Connected Papers**: freemium, $3–15/mo, committed to a permanent free tier. Lifestyle-scale.
- **ResearchRabbit**: "free forever" + donations (Open Collective), added $10/mo RR+ in 2025, then
  **acquired by Litmaps May 2025** — couldn't stand alone.
- **Litmaps**: the "winner" — won by *acquiring its competitor*, not out-monetizing. 75%-off-for-academics
  + country-parity pricing = the market telling them the buyer's real price is ~$0.
- **VOSviewer**: never tried — free/OSS academic public good from Leiden CWTS since 2009, grant-funded.
- **Structural cause (not execution):** commodity FREE data (all read the same OpenAlex/Crossref) +
  no-budget users (researchers) + discretionary occasional task (lit review) + cloneable view = thin
  monetization, consolidation, or public-good. A vitamin for broke customers. NEVER charge researchers
  $5/mo for the OpenAlex explorer — that race is run, 4×. The explorer is our VOSviewer: free public
  good → attention/credibility → funnel.

### How REAL data engines monetize (verified) — the toll booth is hosting, not licenses
- Graph DB market $507M (2024) → $2.1B (2030), 27% CAGR. Vector DB → $4.3B by 2028. Real, growing,
  NOT hyperscale — **Neo4j leads at only ~$200M after 15 years.** Databases are slow, capital-heavy,
  long-sales-cycle, and IBM/Oracle/MS/AWS bundle "good enough" graph/vector into existing stacks.
- Winners monetize via **(1) managed cloud / consumption** (Neo4j Aura, Pinecone $50–500/mo by
  compute+storage+queries, Zilliz Cloud over OSS Milvus) — the dominant, scalable model;
  **(2) open-core** (engine free, charge for clustering/security/SLA/support); **(3) pure license** —
  the WEAKEST, what object-DB vendors tried and mostly failed at.

### Why multidimensional/object engines historically failed — and how we dodge it
- **OODBs (1990s — our closest ancestor: "store the object whole, don't shatter into tables") FAILED:**
  (a) couldn't give data back as plain rows for reports/tools; (b) no SQL / no standard (ODMG group
  disbanded 2001); (c) worse perf than relational at real volumes; (d) underestimated SQL + business
  data processing. **They isolated users from the ecosystem. Isolation = death.**
- **OLAP cubes succeeded then got EATEN by columnar SQL** (ClickHouse/Snowflake/BigQuery compute
  aggregates on-demand from flat tables → pre-aggregation unnecessary). The multidimensional model
  lost to *relational that got fast enough.*
- **"Used secretly in-house" (Palantir, FAANG graph engines, pharma) is a trap, not a market:** those
  who need it most BUILD it and don't buy/share. Proof the need exists; not a sellable segment.

### The 3 hard rules (fall out of the history — design constraints, not slogans)
1. **Don't sell a new query paradigm. Sell a capability through FAMILIAR surfaces.** If adopting nDB
   means abandoning SQL + BI tools, we lose regardless of model quality. **The slicer layer is our
   escape from the OODB tombstone.**

   **VERIFIED 2026-05-29 — what the slicer actually speaks (the OODB tombstone is dodged at the
   architecture level, BUT there's a gap to close):**
   - `crates/ndb-slicer` (1447 LOC) is a real SQL-shaped projection/aggregation engine over a
     `Record` stream: `select` columns, `filter`, `group_by`, aggregates (Count/Sum/Avg/Min/Max/
     **Percentile**), `having`, multi-key `sort`, `limit` → returns a `Table { headers, rows }`.
     That's SELECT/WHERE/GROUP BY/HAVING/ORDER BY/LIMIT semantics — familiar to anyone who knows SQL.
   - `crates/ndb-arrow` (881 LOC) is the **killer escape hatch**: `records_to_batch` → Apache Arrow
     `RecordBatch`, `records_to_ipc_stream` → Arrow IPC bytes. Module doc names the consumers
     verbatim: **"Polars, pandas (via pyarrow), DuckDB, anything else that speaks Arrow."** Arrow is
     THE modern standard interchange — this is exactly "point pandas/DuckDB at it, learn nothing new."
     Denormalised schema (one column per observed `(kind,type_id,property_id)`), identity-column prefix,
     `Vector→List<Float32>`, roles flattened to `List<Struct>`. Real, not aspirational. arrow 55.x dep.
   - **THE GAP (must close before the engine pitch is real):** (a) `ndb-arrow` has NO consumers yet —
     `grep` shows only itself in Cargo deps; it is NOT wired into `ndb-server` (server speaks JSON
     per-request, not Arrow) or `ndb-cli`. So the capability EXISTS as a library but is not EXPOSED as
     an endpoint/command a buyer can hit. (b) the slicer's `Table` has no `to_csv`/`to_json`/`to_arrow`
     serializer and the slicer does NOT depend on ndb-arrow — the two halves (SQL-shaped slicing,
     Arrow output) aren't joined. (c) no Parquet (arrow-ipc only; Parquet would need `parquet` crate).
     (d) input is a query API, not a SQL string — no SQL *parser*, so "point a literal `SELECT` at it"
     is false today; it's a Rust builder API. **Verdict: the BONES beat the OODBs (Arrow = standard,
     not bespoke), but the wiring (server Arrow endpoint + slicer→Arrow + optional SQL-string front)
     is unbuilt. This is a concrete, small-ish engine task, NOT a research risk.**
2. **Toll booth = managed cloud / consumption, NOT a binary license.** Going engine = signing up to
   run a cloud service eventually. Know this now; it's a bigger commitment than "license to pharma."
3. **Need the 10× wedge relational-getting-faster CANNOT close.** Columnar ate OLAP; pgvector eats
   standalone vector DBs for small cases. The one durable composite Postgres+extensions does badly:
   **deep variable-hop graph traversal + as-of-time-travel + vector kNN, composed in ONE query.**
   A prettier explorer is not the wedge; raw n-D storage is not the wedge (OODBs had it and died).
   That composite is. Engine work must aim at THAT 10×, not just "make it fast."

### The new option AI unlocks: engine + hosting + APP (one company, was two)
Comps split into engine vendors (Neo4j: sell infra, customer builds app — can't capture the app layer)
and app vendors (Connected Papers: one app, no engine — can't capture the infra layer). Each gave up
half the value because building both was two companies' work. **AI coding collapses that cost** → nDB
can be the engine vendor AND ship the vertical app, each app both a product AND a live demo selling the
engine beneath. OpenAlex explorer = the *template* for "engine+hosting+app per vertical."

### Where the toll physically sits (combining the above)
- **Hosting/consumption** — meter compute+storage+queries (proven; cloud-ops commitment).
- **Vertical app subscription** — the explorer for BUDGET-HOLDERS (pharma/finance/IP), not researchers.
- **Slicer as a paid boundary** — free to EXPLORE in the n-D view; pay to EXTRACT/slice out to your
  SQL/BI/pipeline at scale. The view is the funnel; getting data into your workflow is the paid action.
  (Mirrors Connected Papers gating graph *count*, but we gate *extraction/scale* — which budget-holders
  need and researchers don't.)
- **Discipline:** prove the loop end-to-end on ONE paid vertical first (proprietary data + budget =
  pharma or finance, NOT academia). Open science = the free attention proof. The first *paid* vertical
  is the real decision, deferred to before M2 — don't be the engine for three verticals at once.

## Who it's for (the user rejected "name one vertical" — deliberately)

Everyone who looks at the world. The attention play is horizontal: researchers, R&D/CI teams,
funders, journalists, librarians — AND students and children, for whom interactive n-D worlds are
play, not work. The brain trained on richer-dimensional observation levels up its ability to
recognize structure in a universe that is fundamentally n-dimensional. Monetization concentrates
later in the budget-holding subset (b); attention is won broadly first.

## Roadmap

### Milestone 1 — Engine survives 10GB+ (THE GATE; next session)
The server must serve a 10GB graph at <1s/tile. Concretely:
- **Server-side top/clusters cache** so the default tile is O(1), not a 46s reverse-scan over 408
  uncompacted sidecars. Plus **sidecar compaction** (408 → 1).
- **On-disk (bounded) HNSW** so kNN is O(log N), not a 15s full scan. (In-RAM HNSW costs RAM — not
  bounded; must be on-disk. Note: `ndb-index-vector-hnsw` depends on `ndb-engine`, so approx/auto
  lives in the APP layer, engine stays exact-only.)
- RAM stays bounded (already proven: open 14s/216MB, steady ~540MB at 10.16GB synthetic).
Until this lands, the explorer at 10GB falls back to the static graph.json. The committed 2.5k
real-OpenAlex demo is unaffected (small, fast).

#### M1 first cut — LANDED 2026-05-29 (commits 21bb8f2 + 4426ee7)
- **Server-side top-cited cache** (`langgraph-server`): pre-rank top-20k cited papers ONCE at startup
  → `<db>/top.json` (uuid + coarse field + year + citations; ~1.6 MB, bounded at any graph size).
  `/view/top` (default tile) and `/view/cluster/*` are now an **O(cache) slice (~5-10 ms)** instead of
  a live `property_top_k` per request. `as_of` time-travel + per-field filtering run on the cached
  year/field in place. Persisted → instant restart. (Clusters were already cached via `clusters.json`.)
- **Sidecar compaction** (`langgraph-ingest --compact <db>`): merges the per-flush SSTables + their
  `.pidx/.vidx/.idx` sidecars (hundreds at 10 GB) down to **one** via the engine's snapshot-aware
  `compact()`, so `property_top_k`/`vector_search` become single-source. Re-declares the indexed
  `(type,prop)` pairs first (else compaction deletes the old sidecars and writes none → reader silently
  RAM-rebuilds; caught + fixed in test). Invalidates a stale `top.json`.
- **Verified at 700k synthetic** (3 SSTables): default tile 5-10 ms; compaction 3→1 SSTable preserves
  all 1.4M records with **byte-identical top/cluster fingerprints** pre vs post; server RSS 302 MB.
  Serving a cached tile is O(cache)+O(limit snapshot_reads), **independent of graph size** — so the
  `<1s/tile at 10GB` target is structural, not just measured-at-700k.
- **kNN at scale — FIXED via a global current-vector snapshot (2026-05-30, commits in ndb-engine).**
  Measurement reframed the problem: kNN's slowness was NOT the vector scan (0.1 s at 3M) but the
  per-SSTable fan-out — `vector_search` gathers candidates from every `.vidx` sidecar then MVCC-verifies
  each (O(sidecars×k) random reads). Fix: `engine.build_vector_snapshot(property)` collapses all CURRENT
  vectors into ONE mmap'd `.vsnap` (bounded TWO-PASS streaming build — no 2×N RAM spike);
  `vector_search_snapshot` reads only that file (no fan-out, no verify). `langgraph-server --knn snapshot`
  (auto prefers it). **Measured at 17M / 68 sidecars / 10.16 GB (`--bench-knn`):** exact cold 1.96 s /
  warm 1.12 s → snapshot cold **0.35 s** / warm **0.30 s**, `same_set=true` 20/20 (exact, no recall
  loss), committed RAM (RssAnon) 150 MB / 2 MB, build 17M vecs in 96.5 s @ RssAnon 2 MB. Snapshot is
  O(1) in sidecar count (exact scales with it → ~40× at the old 408-sidecar config). **Note `VmRSS` is
  misleading here** — the engine's committed memory is RssAnon (≤150 MB at 17M); the multi-GB `VmRSS`
  is reclaimable file-backed mmap, NOT a cap violation. (in-RAM HNSW `--knn approx` remains for
  ≥95-99%-recall when vectors fit RAM; exact multi-sidecar kept as fallback.)
- **Still pending for M1:** (a) **heavy index builds block server startup** — both the top-cache and
  the `.vsnap` are built lazily in `Index::build`, so a cold 10 GB serve waits minutes (the top-cache's
  `property_top_k` over 68 sidecars + ~1.36M verifies took >6 min at 17M and never bound the listener
  in a 90 s health-wait). FIX: make them explicit OFFLINE build steps (a `--build-indexes` CLI, like
  `--compact`) so serving just loads + binds fast. This is the next concrete task. (b) confirm the
  absolute 10 GB `/view/top` tile latency once (a) lands. (c) **compaction RAM caveat:** `compact()`
  builds the record set in RAM during the merge → needs ~DB-size RAM. Fine as one-time offline
  maintenance on a sufficient box; a true bounded-RAM (external-merge) compaction is future work.

### Milestone 2 — Real OpenAlex proof (the wedge made real)
**IN PROGRESS 2026-05-29:** acquisition started — `langgraph-ingest --spool-sharded` (commit 3cd6b7c)
is fetching the citation-backbone slice (`cited_by_count:>50`, ~12.3M works) via the API with
server-side filter + select-projection + on-wire gzip + 10 parallel year-shards. NOTE: switched from
the S3 full-snapshot plan to the filtered API — S3 has no server-side filter, so a coherent slice from
the 639 GB snapshot means downloading all 639 GB; the API downloads only the ~12.6 GB kept.

#### 2 GB RAM cap — architecture made to fit (commit 1050459)
The app has a hard ~2 GB RAM cap. Three paths were over it; two fixed, one constrained:
- **Spool downloader** (was 4.3 GB): 10 shard threads each retained a 50k-work `String` buffer
  (~250 MB, `clear()` kept capacity). Fix: `SPOOL_BATCH_WORKS` 50k→10k + `buf = String::new()` after
  flush + stop cloning each page. Now **0.5 GB** steady (verified 0.04→0.54 GB). Restart resumes from
  per-shard checkpoints.
- **`--from-spool` ingest** (was 4.7 GB, unbounded): the `author_ids` map grew without bound. Fix:
  ingest the **citation backbone only** (papers + CITES, no authors — the explorer renders
  papers/clusters/cites, not authors), and store NO id→EntityId map — derive each EntityId
  deterministically from the work number (`eid_for` = `Uuid::from_u128`), keeping only a
  `HashSet<u64>` membership set (~16 B/entry → ~200 MB at 12.3M). Engine cache capped 512 MB. Verified
  on real OpenAlex (353k papers, 2.3M cites): ingest peak ~0.6 GB, full pipeline
  from-spool→compact(8→1)→serve returns real top-cited (Livak qPCR, Random Forests, GLOBOCAN, graphene)
  + working semantic kNN; server 0.31 GB.
- **Compaction at 10 GB — still over cap (constraint, not yet fixed):** `compact_with_floor` builds the
  whole record set in RAM → needs ~DB-size RAM. Fine for the 0.52 GB test DB; **cannot run on a 10 GB DB
  under 2 GB.** Mitigation: the M1 **top cache makes compaction unnecessary on the hot path** (default
  tile is O(cache), built once over however-many sidecars + persisted). So the 2 GB-capped 10 GB serving
  pipeline is **`--from-spool` → `langgraph-server` (skip `--compact` at scale)**; the one-time top-cache
  build over many sidecars is slower but bounded. `--compact` is only for DBs that fit RAM. True
  bounded compaction = external-merge (streaming k-way) — future engine work. kNN at 10 GB stays
  slow-but-bounded until on-disk HNSW (M1 b) lands.

Then the explorer over real data, served by the M1 engine within 2 GB, is the first credible public
artifact of the thesis. (Original plan referenced the S3 snapshot; superseded by the filtered API above.)

### Milestone 3+ — Discover applications
n-D store is general. OpenAlex viz is application #1. Others to discover (NOT committed): any domain
whose objects are natively high-dimensional and currently shattered into tables — molecular/protein
graphs (the AlphaFold demo already gestures at this), knowledge graphs, financial/temporal networks,
anything where "the JOIN is the lie."

## What I noticed about how you think
- You rejected "name one user" on purpose: "who doesn't want to play with a beautiful interactive
  world? how about children and students?" — you're betting on horizontal attention, not a vertical
  beachhead. That's a real strategic stance, not a dodge (but it raises the monetization bar, which
  is why premise 4 names the toll booth).
- "relationship database human invented 50 years ago is just 2d projections of real world data...
  now computational power is different, we should change" — you start from first principles about
  *why* the incumbent exists, then ask if its constraint still holds. That's the right altitude for
  a platform bet.
- "it's now just the beginning of nDB... imagine how postgres start?" — you hold the OpenAlex viz
  loosely ("just one try") and the platform tightly. Correct: the proof is disposable, the thesis is not.

## The assignment (one concrete action for next session)
**Build Milestone 1's first cut: the server-side top/clusters cache + sidecar compaction**, so a
10GB graph serves the default tile in <1s instead of 46s. That single number — first-tile latency
at 10GB — is the proof the whole thesis stands on. Get it under 1s and the explorer (and every future
application) becomes real. Everything else waits behind it.

## See also
- memory: ndb-2026-05-29-explorer-10g-test (the 46s wall + RAM proof)
- memory: ndb-next-real-openalex-10g (S3-snapshot acquisition plan)
- memory: ndb-2026-05-29-lowram-coreb (engine, knn modes, the schema-contract gotcha)
- memory: ndb-langgraph-demo (the explorer, tile-manager, static fallback)
