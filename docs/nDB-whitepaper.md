# nDB — A Database for N-Dimensional Reality

**An n-dimensional hypergraph database engine built in Rust**

**Status:** Architectural foundation complete; v1 implementation pending
**Date:** 2026-05-27
**Authoritative design:** [`docs/superpowers/specs/2026-05-27-nDB-hypergraph-design.md`](superpowers/specs/2026-05-27-nDB-hypergraph-design.md)

---

## Executive Summary

Most real-world entities are intrinsically **multi-dimensional**. A chemical reaction involves reactants, products, catalysts, temperature, pressure, and solvent — one atomic event with six or more participants. A medical diagnosis interlocks patient, symptoms, candidate pathogens, treatments, and contraindications. A business approval involves participants, documents, time, jurisdiction, workflow, and audit trail.

Traditional relational databases force these multi-dimensional events into 2D tables, fragmenting them across many rows and many tables joined by foreign keys. The fragmentation is not a quirk — it is a structural mismatch between the data model and reality.

**nDB stores n-dimensional reality as it is.** Data is stored as a hyperedge web: entities with radiating connections of arbitrary arity, no flattening. Schemas live inside the data as metadata hyperedges, not as a separate primitive. Time and history are first-class through append-only storage with MVCC. Computation and visualization are composable companion crates, not engine-bound.

Three target application clusters share this foundation:

1. **AI agent reasoning and LLM context** — GraphRAG, knowledge graphs that grow as LLMs ingest, vector + structured queries in one engine
2. **Multi-party business workflows** — ERP, supply chain, regulatory compliance, where business events involve many participants and audit trails must be preserved
3. **Provenance, lineage, and multi-perspectival data** — scientific reproducibility, agricultural traceability, multi-jurisdiction accounting

nDB is shipped as a small Rust engine plus opt-in companion crates. The wire protocol is HTTP + JSON + JSONL, so any language can use the engine. The engine itself is CPU-only and hardware-neutral; GPU acceleration arrives in v2 as opt-in plugin crates (vector indexes, columnar aggregation, GPU slicers) without changes to the engine.

This document is the public introduction. The full architectural design — including byte-level record layouts, MVCC semantics, file format, query DSL grammar, and concurrency model — lives in the design spec referenced above.

---

## The Problem: Reality Is Multi-Dimensional; Databases Are 2D

Relational databases were designed in the 1970s for tabular data — payroll rows, inventory lists, ledger entries. The relational model brilliantly normalized data into tables of rows and columns, and SQL gave the world a powerful query language that has lasted half a century.

But applications have outgrown that data model. The events modern applications care about are rarely 2D.

Consider a single business approval event:

```
Alice approved Sales Order SO-001 on Tuesday 15:00
using the fast-track workflow, with outcome "approved",
comment "all conditions met"
```

This is ONE fact about FIVE participants (Alice, SO-001, Tuesday, fast-track, approved) plus metadata. To store this in SQL you need at minimum:
- An `approvals` table with a synthetic primary key
- Foreign keys to `users`, `documents`, `workflows`
- A separate `audit_log` table that tracks who made the assertion
- An ORM layer in your application that reconstructs the event from these fragments

To answer "find all 5-way Alice approvals using fast-track in 2026 Q1" you write a 5-way JOIN. To audit "what was the schema of approvals in 2024?" you restore a backup. To extend the model with a new dimension (say, "geographic jurisdiction") you write a migration, take downtime, and update every consumer.

These pains are not edge cases. They are the daily reality of building modern applications on relational databases.

Property graphs (Neo4j, TigerGraph) help somewhat — they put relationships first — but their edges are still **binary**. A 5-arity fact like the approval above requires "reification": creating a stub `Approval` node and connecting 5 binary edges to it. The fact's atomicity is lost. Querying it requires multi-hop traversal. LLMs producing structured facts have to navigate the reification machinery.

Document stores (MongoDB) allow flexible per-document shapes, but they don't model **relationships** well — joins are awkward, traversal is foreign, and schema discovery from data is bolt-on.

Triple stores (RDF / SPARQL) handle n-ary via reification (or RDF-star with quoted triples) but still operate at 3-arity natively.

Hypergraph databases (TypeDB, HyperGraphDB) get the n-ary right but TypeDB requires upfront schema declaration (excluding the AI/extraction use case) and HyperGraphDB is niche and dated.

**The category gap nDB targets:** a hyperedge-native database that is also schemaless-friendly, optimized for AI / ERP / scientific workloads, and architecturally minimal so the ecosystem can extend it.

---

## What nDB Is

A production-grade hypergraph database engine in Rust. Five foundational architectural commitments:

### 1. Hyperedge-native data model

Every fact in nDB is a **hyperedge** — a connection between any number of named role-players, with optional properties. There is no reification, no synthetic intermediate node, no foreign-key choreography.

The approval event above becomes:

```
HyperEdge {
    type: "approval"
    roles: {
        document:  SO-001,
        approver:  Alice,
        timestamp: 2026-05-26T15:00,
        workflow:  fast-track,
        outcome:   approved
    }
    properties: { comment: "all conditions met" }
}
```

One atomic fact. One write. One traversal. No reification.

Chemistry, biology, business events, knowledge claims — all fit the same primitive. **Nested entity hierarchies** (body contains cells contains proteins contains amino acids; document contains sections contains paragraphs; filesystem contains directories contains files) are also expressed as standard hyperedges with a `contains` relation type. Cascade lifecycle (what happens to children when a parent is deleted) is declared per-hyperedge with a sensible default that matches biological intuition. Recursive / transitive queries traverse the hierarchy at any depth with a single query.

Section 5 of the design doc walks through the data model in detail, including the containment pattern.

### 2. Schema as metadata, not a separate primitive

In nDB, "schema" is not a separate engine concept with its own language and machinery. It is **the collection of metadata hyperedges that describe other hyperedges**. Type assertions, constraints, index declarations, inference rules — all are hyperedges written with the same query DSL as any data.

```
write type_def(name: "Customer",
               required_properties: ["customer_code", "name"])

write constraint(target_type: "Customer",
                 rule_expr: "matches(tax_id, '^[0-9]{10,13}$')")
```

This means:

- **No CREATE TABLE.** No ALTER TABLE. No migration ceremony.
- **Schema evolves at the speed of data.** New types, new constraints, new indexes — just write more hyperedges.
- **AI agents can extend the schema.** An LLM extracting facts can write `type_def` hyperedges as it discovers concepts.
- **Schema queries use the same DSL as data queries.** No separate INFORMATION_SCHEMA.
- **Schema is MVCC-versioned.** "What did our schema look like last year?" is a normal time-travel query.
- **Schema is auditable.** Every schema change carries the same provenance metadata as any data change.

Apps can choose enforcement strictness per type: schemaless, soft-validated, strictly-validated, or fully reasoned over. The same engine serves AI extraction (loose) and ERP audit (strict).

### 3. Append-only storage with MVCC

The storage core is append-only. Updates are new assertions superseding old ones. Deletions are tombstone assertions. The current state is always the most recent assertion. Compaction removes old versions per per-type retention policies.

This cascades into several wins:

- **Provenance is free.** The history *is* the storage.
- **MVCC is natural.** Every write already creates a new version; no separate version-tracking machinery.
- **Time-travel is a feature.** `as of 2025-12-31 match customer(id: ?c, balance: ?b)` works natively.
- **Audit trail is free.** Every assertion records who, when, and from what source.
- **LSM-friendly.** Append-only maps cleanly to log-structured merge trees, which is what production write-heavy databases (RocksDB, Cassandra) use.
- **GDPR-compatible.** Selective deletion via compaction supports right-to-be-forgotten.

The storage bloat that naive append-only databases suffer is addressed through per-type retention policies (audited / versioned / latest-only), hot/cold tiering, and standard block compression. Realistic ERP workload (100M business facts/year, 5-year retention): ~100 GB hot, ~$2/month cold archival. Roughly 3× the storage of an update-in-place database, in exchange for free audit, MVCC, and provenance.

### 4. Engine retrieves; slicers compute

The engine is small. It does:

- Primary storage (append-only LSM, custom binary format)
- Pattern matching, filtering, projection, time-travel
- MVCC transactions (snapshot isolation by default; serializable available per transaction)
- The index framework + a handful of mandatory built-in indexes
- Compaction and retention enforcement

The engine does **not** do aggregation, sorting, grouping, or computation. Those live in **slicer crates** — composable companion crates that build on the engine's retrieval primitives.

A slicer is also a **declarative projection** from the n-dimensional hyperedge graph onto a k-dimensional visual space, following the grammar-of-graphics tradition (Wilkinson, 1999) extended from tabular to hypergraph data.

This split has three consequences:

- **The engine stays minimal.** Smaller surface area, easier to test, fewer bugs.
- **Computation is pluggable.** Slicers can use Polars, DataFusion, custom Rust code, or GPU compute libraries. Apps choose.
- **Visualization is declarative.** A slicer declares which graph dimensions map to which visual variables (position, color, size, shape, motion, opacity). The renderer just renders. The same data renders as a table, a 3D scatter, a sankey, a network, or a heatmap depending on which renderer the app composes.

For workloads where heavy aggregation is critical, columnar index plugins (v2) expose aggregation as a plugin-specific API; the slicer pushes aggregation down to them automatically.

### 5. Plugin framework everywhere — including GPU

The architecture is a small mandatory engine plus opt-in plugin crates. Indexes are plugins. Slicers are plugins. Renderers are plugins. Vector similarity, full-text search, geographic indexes — all plugins.

This means:

- **The engine doesn't ship every feature.** It ships the framework. The ecosystem ships the features.
- **GPU acceleration is just another set of plugins.** `nDB-index-vector-cuda` for GPU vector search. `nDB-index-columnar-cuda` for GPU aggregation. `nDB-slicer-cuda` for GPU compute. The engine compiles and runs without any GPU toolchain; GPU arrives in v2 as opt-in dependencies.
- **Apps choose hardware per workload.** Need GPU vector search? Add the CUDA plugin. Running on edge / mobile? Use only CPU plugins. Same engine.
- **The community can extend the engine.** A chemistry app can ship a similarity-on-molecular-structure index. A geospatial app can ship an R-tree index. No engine fork needed.

The plugin trait (sketched in Section 13.2 of the design doc) lets each plugin report cost estimates for queries. The query planner uses these to dispatch — GPU plugin for large workloads, CPU plugin for small ones — automatically.

---

## Three Worlds Where This Matters

### 1. AI agent reasoning and LLM-driven knowledge graphs

LLMs naturally produce n-ary structured facts when given the freedom to. "Alice gave Bob a book on Tuesday in the library" is one mental fact for an LLM — and it's one hyperedge in nDB. Current GraphRAG pipelines work hard to bend LLM output into reified property-graph triples or constrained SQL schemas. nDB matches what LLMs produce natively.

Concrete capabilities:

- **Emergent schemas.** An LLM ingesting PubMed papers discovers `clinical_trial` as a recurring concept. It writes a `type_def`. As more examples accumulate, it adds constraints. Schema firms up as data accumulates. No migrations.
- **Vector + structured queries together.** Vector index plugin (v2) sits alongside structured-pattern queries. "Find documents similar to this embedding AND mentioning entities of type Drug" is one query, one engine.
- **Provenance baked in.** Every LLM-extracted fact carries who-extracted, when, from-what-source. Hallucination grounding becomes a query, not a custom audit system.
- **Time-travel for retraining.** "What facts did the system believe before model v3 was deployed?" is `as of T match ...`. Useful for model auditing, A/B comparison, regression analysis.

For domains like medical reasoning, scientific literature analysis, legal case analysis, and autonomous agent context management, nDB is built for the workload that AI is creating, not the workload SQL was designed for in 1974.

### 2. Multi-party business workflows

Vietnamese accounting (TT200/2014 → TT99/2025 transition), international trade (5-party deals across buyer / seller / bank / customs / shipper), multi-stakeholder approvals, manufacturing BOMs — these are inherently multi-party events. SQL forces them into normalized fragments; nDB stores them as hyperedges.

Concrete capabilities:

- **Multi-party events as atomic facts.** A 3-way invoice match (PO + GRN + Invoice) is one hyperedge, not three rows + a match table.
- **Per-tenant schema variations.** Each tenant defines its own `Customer` type by linking the type definition to the tenant entity. No tenant pollution, no separate databases per tenant.
- **Regulatory time-travel.** When VAS rules change, old data is still queryable under old schema. Auditors in 2030 can reconstruct 2024 state without backups.
- **Audit trail free.** Every change is MVCC-versioned with provenance. Compliance reporting becomes a query.
- **Multi-jurisdiction views.** The same financial event recorded under Vietnamese VAS + IFRS + parent-company GAAP simultaneously, via multiple type_defs and constraints. No data duplication.

For ERP, supply chain, compliance, and any business system where the events involve multiple parties and audit matters, nDB removes structural friction that SQL imposes.

### 3. Provenance, lineage, scientific reproducibility

W3C PROV-O lineage facts are 6+-arity: `(output, derived_from, inputs..., by_transformation, at_time, with_parameters)`. nDB stores these natively without reification. Agricultural traceability (EUDR compliance), scientific result reproducibility, data lineage in ML pipelines, multi-jurisdiction accounting — all fit the same model.

Concrete capabilities:

- **One hyperedge per lineage event.** A computational result links to inputs, transformation, parameters, compute, timestamp, researcher — atomically.
- **Federated provenance.** Multiple nDB instances can reference each other's facts via stable identifiers + lookup keys (v3 federation).
- **Versioned/branched knowledge.** TerminusDB's killer feature, generalized to hyperedges. "As of git-commit-X, this is what we believed."
- **Cross-perspective queries.** Multi-jurisdiction views, multi-observer perspectives — different metadata hyperedges describing the same underlying facts.

---

## How nDB Compares

| Dimension | SQL (Postgres/MySQL) | Neo4j (property graph) | TypeDB (hypergraph) | Datomic | MongoDB | nDB |
|---|---|---|---|---|---|---|
| **N-ary facts native** | No (joins) | No (binary edges + reification) | Yes | Partial (datoms are 4-tuple) | Document-shaped | **Yes** |
| **Schemaless option** | No | Partial (schema-on-read) | No (schema-strict) | No | Yes | **Yes (schema is opt-in)** |
| **Schema is data** | No | No | No | Yes (datoms) | No | **Yes** |
| **Time travel native** | No (point-in-time recovery only) | Partial | No | Yes | No | **Yes** |
| **Audit trail free** | No (custom triggers) | No | No | Yes | No | **Yes** |
| **AI-extracted schema** | No | Partial | No | Partial | Yes (but schemaless = no validation) | **Yes (typed + flexible)** |
| **Visual projection built-in** | No | No (separate tools) | No | No | No | **Yes (slicer crates)** |
| **GPU acceleration path** | Limited (pg_strom extension) | None native | None | None | None | **Plugin-based, v2** |
| **Any-language clients** | Yes | Yes | Yes | Limited | Yes | **Yes (HTTP + JSON + JSONL)** |
| **Plugin extensibility** | Extensions (heavy) | Limited | Limited | Limited | Limited | **First-class** |
| **MVCC** | Yes | Limited | Yes | Yes | Limited | **Yes (SI + SSI per transaction)** |
| **Storage durability** | Mature | Mature | Mature | Mature | Mature | **v1 target** |

nDB's positioning: hypergraph-native (like TypeDB) but schemaless-friendly (like MongoDB) with time-travel and audit (like Datomic) and a plugin ecosystem (like PostgreSQL extensions, but designed-in rather than retrofit).

---

## Performance Characteristics

The architectural decisions shape performance characteristics:

**Strong:**

- Point lookups (entity by ID, hyperedge by ID): comparable to RocksDB / LevelDB performance — sub-millisecond on hot data
- Pattern traversal (graph queries): comparable to property graphs for binary patterns; faster for n-ary because no reification hops
- Time-travel queries: same cost as current-state queries (MVCC + snapshot reads)
- Append throughput: single-writer + batching, RocksDB benchmark territory (10K+ writes/sec per writer)
- Read concurrency: lock-free MVCC; many readers don't contend
- Vector similarity (v2 with GPU plugin): cuVS / FAISS-GPU performance class
- Streaming subscriptions: append-only log makes change-feed trivial

**Honest trade-offs:**

- Ad-hoc aggregation without prepared indexes: slower than DuckDB / ClickHouse / BigQuery, because raw rows stream from engine to slicer for aggregation. Mitigated by columnar index plugin (v2) when available.
- Multi-writer throughput in v1: single-writer + batching. Multi-writer revisited in v3+ alongside distribution.
- Distributed transactions: v3+. v1 is single-node.

For nDB's target workloads (AI reasoning, ERP, scientific), the strong-performance scenarios dominate. The trade-offs are explicit in Section 14 (Non-Goals) of the design spec.

---

## Roadmap

### v1.0 — Initial Production Release

Goal: usable single-node engine for one workload (AI reasoning or ERP, decided closer to launch).

Storage core, MVCC, retention policies, query language (retrieval-only), six mandatory indexes (entity-by-ID, hyperedge-by-ID, lookup-key reverse, adjacency list, hyperedge-type clustering, schema-driven property B-tree), companion crates for 2D rendering, wire protocol clients in Rust. One real-world pilot.

### v2.0 — AI + Analytics + First GPU Support

Goal: viable for AI / GraphRAG and slicer-heavy analytics, with GPU acceleration.

Vector index (CUDA), columnar aggregation (CUDA + CPU), slicer materialized views, full-text index, GPU slicer crate, pinned memory pool, Arrow IPC interop, 3D + 4D renderers, schema layer 3 (constraints), Python + JavaScript clients.

### v3.0 — Distribution + Ecosystem + Cross-Platform GPU

Goal: differentiated from competitors, ready for broader adoption.

Schema layer 4 (ontology + inference), read replicas, federation, cross-platform GPU plugins (ROCm + Metal + wgpu), 5D + 6D renderers, Go + Java clients, public plugin API, community-contributed plugins.

### v4.0+ — Distributed and High-Dimensional

Goal: web-scale write workloads + saturating the visual variable hierarchy.

Sharding, Raft-replicated state, multi-region, 7D + 8D renderers (approaching the cognitive ceiling), GPUDirect Storage paths considered.

Sections 16 of the design spec details the per-version scope and success criteria.

---

## What nDB Is Not

Honest exclusions, so adopters know when to choose something else:

- **Not a SQL replacement** for high-volume tabular aggregation. SQL on Postgres / MySQL remains the right tool for rigid-schema ledger workloads.
- **Not an OLTP system for high-frequency trading.** Million-TPS scenarios are out of scope.
- **Not a document store.** Documents (JSON blobs) are anti-pattern; nDB wants entities and hyperedges, not opaque payloads.
- **Not a search engine.** Full-text search exists as an opt-in plugin but is not the primary access pattern.
- **Not a streaming engine.** nDB ingests events but doesn't process streams with stateful operations (use Flink, Kafka Streams).
- **Not an ad-hoc OLAP engine without index preparation.** Sub-second analytical queries over raw data without prepared columnar indexes go to DuckDB, Snowflake, BigQuery, or ClickHouse. nDB is competitive when aggregation-capable indexes exist; without them, falls back to streaming + slicer-side aggregation, which is slower.

---

## Risks We Are Honest About

- **Query language fragmentation.** Inventing a new DSL is correct but raises adoption friction.
- **Ecosystem competition.** Neo4j has years of mature tooling. We're catching up.
- **Algorithm gap.** Classic graph algorithms (PageRank, shortest path) need redefinition for hyperedges. This is research, not just engineering.
- **TypeDB precedent.** They tried hyperedge-native and adoption is slow. We must understand why before assuming we'll do better — their schema-strict requirement is part of it; we keep schemaless-core which we believe broadens adoption.
- **Effort scale.** Production-grade DB engine in Rust is a 100k+ LOC multi-year commitment.
- **Append-only storage cost.** ~3× the storage of update-in-place. Acceptable given modern storage costs, but worth monitoring.

---

## Prior Art and References

**Foundational:**
- Berge, C. *Graphes et hypergraphes* (1970)
- Wilkinson, L. *The Grammar of Graphics* (1999)
- Mackinlay, J. "Automating the design of graphical presentations" (1986)
- Cahill et al. "Serializable Isolation for Snapshot Databases" (SIGMOD 2008)

**Existing hypergraph databases:**
- TypeDB (formerly Grakn) — schema-strict only; nDB makes schema opt-in
- HyperGraphDB (2007) — niche, embedded
- GraphBrain — NLP-focused

**Inspirations:**
- Datomic — schema as data, append-only, MVCC
- Wikidata — opaque IDs + lookup keys
- TerminusDB — git-style versioning of RDF
- Neo4j — property graph mindshare leader (what to learn from, what to differ from)
- RDF / OWL / SHACL — layered schema model on triples; we generalize to hyperedges
- PostgreSQL — MVCC reference implementation; mature tooling to learn from
- CockroachDB / FoundationDB — Rust-adjacent transactional systems
- RocksDB / LevelDB — LSM tree implementations

**Methodologies and conventions:**
- LSM trees (Patrick O'Neil et al., 1996)
- UUID v7 (IETF draft, time-ordered UUIDs)
- Apache Arrow + Parquet (columnar interchange)

---

## Status and Engagement

The architectural foundation is complete and committed to git. The next phase is implementation — starting with the v1 storage core prototype.

Contributors interested in:
- Implementation in Rust
- Performance benchmarking against Neo4j, TypeDB, Datomic
- Vector / columnar / full-text plugin development
- Schema / type system design
- GPU plugin development
- Language client development (Python, JS, Go, Java)
- Real-world pilot applications (AI, ERP, scientific)

...are welcome. The design spec is available alongside this paper. License, contribution model, and governance to be announced.

---

*The full architectural design — including byte-level record layouts, MVCC implementation details, file format, query DSL grammar, concurrency model, and the index plugin trait — is in [`docs/superpowers/specs/2026-05-27-nDB-hypergraph-design.md`](superpowers/specs/2026-05-27-nDB-hypergraph-design.md).*

*End of white paper.*
