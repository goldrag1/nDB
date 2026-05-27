# nDB — N-Dimensional Hypergraph Database Engine

**Status:** Draft (architectural foundation; ready for review)
**Date:** 2026-05-27
**Language:** Rust
**Project root:** `/home/long/long/nDB-ndimemsion-database/`

---

## 1. Vision

Most real-world entities are intrinsically multi-dimensional.

- A **chemical reaction** involves reactants, products, catalysts, temperature, pressure, and solvent — one atomic event with six or more participants.
- A **gene-expression event** involves a gene, its transcription factors, co-factors, the chromatin state of its locus, the time-point, the cell type, and the resulting protein.
- A **medical diagnosis** involves a patient, presenting symptoms, candidate pathogens, possible treatments, contraindications, and confidence levels — all interlocked.
- An **agricultural harvest** involves a crop batch, geospatial coordinates, harvester, processing facility, date, and downstream compliance jurisdiction (EUDR, organic certification).
- A **business approval** involves participants, document, time, jurisdiction, workflow, and audit trail.

Reality is an n-dimensional web. Different domains differ in *what* the dimensions are, not in *how many* there are or how they interlock.

Relational databases force this n-dimensional reality into 2D tables. The "2D-ness" of SQL is not primarily about endpoint count — it's about rigidity. A SQL row has many columns (many dimensions), but the columns are fixed per table. Adding a new dimension requires altering the table; expressing a fact that doesn't fit the table's schema is impossible without normalization across multiple tables joined together.

nDB stores n-dimensional reality as it is. Data is stored as a **hyperedge web**: entities with radiating connections of arbitrary arity, no flattening. Different "slicers" project this n-dimensional truth into 2D, 3D, or higher-dimensional views for human and machine consumption. The database is the honest record; projections are computed on demand.

This is not a replacement for SQL in all contexts. SQL remains superior for high-volume aggregation, fixed-schema transactional workloads, and mathematical guarantees over flat ledgers. nDB targets workloads where the rigidity of SQL forces costly normalization and where the n-dimensional shape of reality is itself the primary thing being modeled.

---

## 2. Scope

**Production-grade hypergraph database engine in Rust.** Multi-year effort. Comparable in eventual scope to Neo4j, TigerGraph, Dgraph, TerminusDB, and TypeDB — but hyperedge-native from day 1, with a layered/optional schema model that no existing competitor combines.

**Not a research project, not a toy.** The architecture must be defensible for production workloads (durability, consistency, recovery, observability) from the start, even if early versions are single-node and feature-light.

**Estimated effort:** 100k+ lines of Rust over multiple years. This document is the architectural foundation; subsequent specs will decompose into implementable milestones.

---

## 3. Target Applications

A single hyperedge core serves three distinct application clusters. They share storage but differ in query patterns, indexing priorities, and schema strictness.

### 3.1 AI agent reasoning / LLM context (primary near-term opportunity)

- Hot emerging market — GraphRAG, KG-augmented LLMs
- LLMs produce/consume n-ary facts naturally ("Alice gave Bob a book on Tuesday in the library" is one fact, not five binary edges)
- Reification stubs in property graphs add cognitive overhead for LLM reasoning
- No entrenched hyperedge-native leader in this space
- Likely to drive early adoption

Concrete reasoning domains where n-ary facts are the norm:

- **Scientific literature graphs** — "drug X inhibits enzyme Y in pathway Z at concentration C, validated in study S" is one atomic claim
- **Medical diagnostic reasoning** — patient + symptom set + pathogen + treatment + contraindication interlock in single facts; LLM-assisted diagnostic systems benefit directly
- **Genomic data integration** — gene + variant + phenotype + population + study cohort
- **Biochemical pathway modeling** — reaction networks where each step has multiple substrates, products, and conditions
- **Legal reasoning** — case + parties + statutes + jurisdiction + precedent in one decision graph
- **Knowledge engineering for autonomous agents** — situation + actors + resources + constraints + outcomes

### 3.2 Multi-party business workflows (ERP and enterprise)

- 3-way match in accounting: `(PO, GRN, Invoice)` as one atomic fact
- International trade: `(buyer, seller, bank, customs, shipper)` 5-party events
- Multi-signatory approvals as atomic facts (preserves audit trail integrity)
- Manufacturing BOMs with composite conditions
- Genuinely underserved by SQL — every n-ary business event is currently painful to normalize

### 3.3 Provenance, lineage, and perspectival data

- W3C PROV-O lineage facts: `(output, derived_from, inputs..., by_transformation, at_time, with_parameters)` — 6+ arity, native fit
- **Agricultural supply-chain traceability** (EUDR, organic certification) — `(crop_batch, farm_geocoords, harvester, processing_facility, date, certification_status)` traced from field to shelf in a single hyperedge per stage
- **Scientific reproducibility** — `(result_dataset, derived_from_inputs, by_algorithm, with_parameters, on_compute, at_time, by_researcher)`
- **Multi-jurisdiction accounting** (Vietnamese VAS + IFRS + parent GAAP simultaneously) — same financial event recorded with different perspectives
- **Versioned / branched knowledge** (TerminusDB's killer feature, generalized) — "as of git commit X, this is what we believed"
- Provenance as a free byproduct: every hyperedge carries who-asserted, when, from-what-source

---

## 4. Architecture Overview

nDB ships as a small Rust engine plus a set of companion crates that compose into whatever the application needs. The engine is what's strictly mandatory; everything else is opt-in.

```
+------------------------------------------------+
| Application (any language via wire protocol)   |
+================================================+ <-- wire protocol boundary
| nDB Engine (Rust crate, mandatory)             |
| - Primary storage (append-only hyperedge log + |
|   entity store; UUID v7; MVCC; retention)      |
| - Query parser / planner / executor            |
| - Index framework + mandatory built-in indexes |
| - Schema validation hooks (data-driven from    |
|   metadata hyperedges)                         |
| - Compaction                                   |
+------------------------------------------------+
| Companion crates (ship with nDB, opt-in)       |
| - nDB-slicer       (projection API +           |
|                     built-in projections)      |
| - nDB-renderer     (table, 2D, 3D, ...         |
|                     dimensional visualizers)   |
| - nDB-index-*      (columnar, vector, fulltext |
|                     — opt-in index plugins)    |
| - nDB-client-*     (Rust, Python, JS, Go, Java |
|                     wire-protocol clients)     |
+------------------------------------------------+
| Out-of-tree extensions (app or third-party)    |
| - Custom slicers (domain-specific projections) |
| - Custom renderers (specialized visualizations)|
| - Custom indexes (spatial, similarity, etc.)   |
| - Custom clients (any language with HTTP/gRPC) |
+------------------------------------------------+
```

**Boundaries are strict.**

- **The engine boundary is the wire protocol.** Apps in any language talk to the engine through it. The engine is shipped in Rust (for speed and memory safety) but the architecture is language-neutral.
- **Primary storage** is the canonical record. It knows how to append assertions, retrieve them by ID, and compact per retention policies. It does not know about schema validation, indexes, slicers, or rendering as separate primitives — those are either built on top of it (schema, indexes) or downstream of it (slicers, renderers).
- **Indexes are derived structures** consumed by the query planner. The engine ships a small mandatory set; additional indexes plug in via a stable `Index` trait (in-tree or out-of-tree). Schemas, slicers, and apps can all drive index creation.
- **Schema is metadata hyperedges** stored alongside data. The engine's validation hooks read these and enforce per-type rules. Schema is not a separate primitive.
- **Slicers and renderers are companion crates**, not engine layers. The engine doesn't know they exist. Apps compose them above the wire-protocol boundary.

This layering is what lets the same engine serve AI / ERP / provenance / scientific workloads without forking: different apps configure different schemas, register different indexes, compose different slicers and renderers — all over one canonical engine.

---

## 5. Data Model: Hyperedge-Native

### 5.1 Core primitives

- **Entity** — a node. Carries an opaque internal ID and optional properties.
- **Hyperedge** — a fact connecting any number of entities (arity 2, 3, ... N). Carries an opaque internal ID, a type, and named role-player slots.
- **Property** — a typed key-value attached to either an entity or a hyperedge.

A hyperedge is **not** a relation between two endpoints. It is an atomic fact about N participants, each playing a named role.

### 5.2 Why entities and hyperedges are different things

The distinction is foundational. Conflating them — making everything a hyperedge, or everything an entity — breaks down quickly in practice.

**Entity = noun. Hyperedge = sentence.**

A sentence ("Alice approved the order on Tuesday") requires multiple nouns to be meaningful. Nouns exist on their own. You can have a noun in your vocabulary that you haven't used in a sentence yet. You can update what you know about a noun (Alice's age) without changing every sentence she appears in.

The same separation applies in nDB:

- **Entities exist independently.** Alice is a Customer even before any orders. You should be able to create her, update her email, and reference her in many different facts. A lonely entity (no hyperedges yet) is valid.
- **Hyperedges are statements about entities.** They cannot exist without their participants. A hyperedge IS the connection.

Why this matters:

1. **Multi-role flexibility** — The same entity plays different roles in different facts. Alice may be `approver` in one hyperedge, `author` in another, `attendee` in a third. Her identity is constant across all of them.
2. **Different lifecycles** — Entities are long-lived (years for a Customer). Hyperedges are typically immutable once asserted (append-only). Mixing these into one primitive forces a compromise on both.
3. **Different indexing needs** — Entities want ID indexes, lookup-key indexes, per-property indexes. Hyperedges want per-entity adjacency lists, per-type indexes, per-role indexes. Different physical structures.
4. **Different schema rules** — "Every Customer must have a `tax_id`" is an entity rule. "Every Approval must have `document` and `approver` roles filled" is a hyperedge rule. Structurally different.
5. **Hyperedge metadata** — A fact about a fact ("this approval was recorded by the audit log on date X") is cleaner when hyperedges have explicit identity.

**The "everything is a hyperedge" alternative considered.** Datomic's EAV model treats entities as just IDs with facts attached — no entity record, only clouds of (entity, attribute, value) tuples. We borrow opaque IDs and append-only semantics from that lineage but keep the entity/hyperedge distinction because readability, performance, and hyperedge-as-first-class metadata all suffer when collapsed.

**Properties vs hyperedges — the boundary case.**

A property attaches a literal value to an entity or hyperedge. A hyperedge connects multiple entities. The choice:

| Use a property when                       | Use a hyperedge when                              |
|---|---|
| Value isn't shared (Alice's email is hers alone) | Multiple participants are involved              |
| Value is a literal (number, string, date) | The fact itself has metadata (who recorded, when) |
| Query is "give me Alice's email"          | The relationship type is queryable                |

ERP business events lean hyperedge-heavy. Simple descriptive attributes lean property-heavy. Both coexist in any real domain.

### 5.3 Examples across domains

Three examples in three domains — each is one hyperedge in nDB; each would require a reified intermediate node + multiple binary edges in a property graph.

**Chemistry — a reaction:**

```
HyperEdge {
    type: "chemical_reaction"
    roles: {
        reactant_1:   Sodium,
        reactant_2:   Chlorine,
        product:      SodiumChloride,
        catalyst:     Water,
        temperature:  T_25C,
        environment:  Exothermic,
    }
    properties: {
        yield_pct: 98.5,
        observed_in_study: StudyRef_2024_117,
    }
}
```

A reaction is fundamentally n-ary — it doesn't exist without all participants. Reifying loses this atomicity.

**Biology — a gene-expression event:**

```
HyperEdge {
    type: "gene_expression"
    roles: {
        gene:                  TP53,
        transcription_factor:  NF_kB,
        co_factor:             p300,
        chromatin_state:       open,
        cell_type:             hepatocyte,
        time_point:            T_30min_post_stress,
        product:               TP53_mRNA,
    }
    properties: {
        expression_level: 4.2,
    }
}
```

Seven role-players in one atomic biological fact. Querying "all transcription factors that produced TP53 mRNA in hepatocytes under stress" walks one hyperedge type, not a six-table join.

**Enterprise — an approval event:**

```
HyperEdge {
    type: "approval"
    roles: {
        document:  SO-001,
        approver:  Alice,
        timestamp: 2026-05-26T15:00,
        workflow:  fast-track,
        outcome:   approved,
    }
    properties: {
        comment: "all conditions met",
    }
}
```

The fact is atomic across all three domains. Querying "all 5 things involved in this event" walks one edge, not five. Auditing or filtering on any role/property is a single check, not a multi-hop join.

### 5.4 Why hyperedge-native (not property graph + reification)

- Atomicity: n-ary facts are stored as one row, not N + 1 rows requiring reconstruction
- Audit/integrity: reification can be partially constructed; hyperedges cannot
- LLM-friendly: matches the n-ary structure of natural language events
- Traversal: one hop instead of two
- Honest representation: storage shape matches conceptual shape

### 5.5 Trade-offs we accept

- No existing query language fits cleanly (Cypher / SPARQL / Gremlin assume binary)
- Classic graph algorithms (PageRank, shortest path, community detection) need redefinition for hyperedges
- Smaller developer mindshare; longer onboarding
- More design work for query optimizer and indexes

These are real costs. The architectural payoff — honest representation of n-dimensional reality — is judged worth them.

---

## 6. Schema Philosophy: Layered / Optional

### 6.1 What "schema" means in nDB

Schema is **not** a separate engine primitive. It has no own storage, no own language, no own API. It is **the collection of metadata hyperedges that describe other hyperedges** — type assertions, constraints, index declarations, inference rules.

Schemas are written using the same query DSL that writes any data. The engine's "schema layer" is a *consumer* of these metadata hyperedges, not a separate writer.

The word "schema" is retained as familiar shorthand, but it does NOT carry the traditional SQL meaning (separate CREATE TABLE language, ALTER TABLE machinery, system catalog). In nDB it means: "the metadata about how data is shaped, expressed as data."

This is the Datomic pattern, generalized: a schema in Datomic is just a set of datoms describing attributes. In nDB, a schema is just a set of hyperedges describing types, constraints, indexes, and rules.

Apps choose enforcement strictness per type (declared in the `type_def` metadata hyperedge itself). nDB has no namespace primitive — configuration attaches at the granularity of types, transactions, or individual entities, not coarse namespace containers.

### 6.2 The four layers

The "layers" describe **what kinds of metadata hyperedges** exist, ordered by how much the engine does with them. Apps opt into each layer per type.

```
Layer 4: Ontology + reasoning      <- reasoning apps opt in
Layer 3: Constraints + validation  <- ERP, strict mode
Layer 2: Type assertions           <- AI apps, flexible mode
Layer 1: Raw hyperedges            <- storage core, no metadata required
```

- **Layer 1** — Raw hyperedge storage. No metadata required. Engine accepts any hyperedge of any arity with any role names. Fast, simple, universal.
- **Layer 2** — Entities and hyperedges declare types via `type_def` metadata hyperedges. Apps can query "what type is this entity" and "what entities are of type Customer". Drift-tolerant. Used by AI/extraction workloads.
- **Layer 3** — Constraint hyperedges declare rules per type (cardinality, required roles, value domains). Validated at write-time or read-time per app config. Used by ERP.
- **Layer 4** — Ontology hyperedges declare class hierarchies, equivalence, inference rules. Used by reasoning systems.

### 6.3 Enforcement modes (per type)

Declared in the `type_def`'s `enforcement` property:

- **Strict write** — invalid writes rejected (ERP default)
- **Soft read** — invalid reads flagged but returned (AI extraction default)
- **Inference on** — derived facts computed from Layer 4 rules (reasoning systems)

Different types in the same database can run at different strictness levels. A single application can have its financial entities (`Account`, `JournalEntry`) on `strict_write` while AI-extracted entities (`ExtractedFact`) run on `soft_read` — all in one engine instance.

### 6.4 What metadata hyperedges look like (concrete)

A "schema" for a Customer entity type, expressed entirely in hyperedge writes:

```
# Type assertion (Layer 2)
write
  type_def(
    name: "Customer",
    description: "A purchasing entity",
    required_properties: ["customer_code", "name"]
  )

# Constraint (Layer 3)
write
  constraint(
    target_type: "Customer",
    rule_expr: "matches(tax_id, '^[0-9]{10,13}$')",
    severity: "strict"
  )

# Index declaration (schema-driven indexing — see Section 13.2)
write
  index_declaration(
    target_type: "Customer",
    property: "email",
    index_kind: "btree",
    unique: true
  )

# Ontology rule (Layer 4)
write
  inference_rule(
    name: "vip_customer",
    when: "Customer.total_purchases_last_year > 1000000",
    derive: "vip_status(customer: ?c)"
  )
```

These are all just hyperedges. They're written using the same DSL as any other data (Section 12). The engine reads them and:

- Layer 2 → indexes them so type queries work
- Layer 3 → validates incoming writes against constraints
- Index framework → creates the declared B-trees / vector indexes / etc.
- Layer 4 → runs inference at query time

Schemas can be queried like any other data: `match type_def(name: ?t)` returns every declared type. `match constraint(target_type: "Customer")` returns every Customer constraint.

### 6.5 Precedent

This pattern is RDF/OWL applied to hyperedges instead of triples. The Semantic Web stack proves the architecture is viable:

- RDF = schemaless triples
- RDFS = light schema (more triples)
- OWL = full ontology (more triples with semantics)
- SHACL = constraint validation (more triples)

We do the same thing, one arity-level up: schemaless hyperedges + metadata hyperedges describing them.

### 6.6 Why not schema-strict only (TypeDB approach)

TypeDB requires schemas declared upfront, with a separate TypeQL Define language as a distinct primitive. This excludes the AI/extraction use case where types emerge from data. We refuse this exclusion. The schemaless core is non-negotiable; schema-as-metadata is opt-in.

### 6.7 Metadata and data: same primitive

**At the storage level: one hyperedge.** A `type_def` and an `approval` have byte-for-byte the same record layout (Section 11.2), same MVCC fields, same indexing. Storage and engine are unaware of any metadata/data distinction.

**At the semantic level: a useful role distinction.**

- **Data hyperedges** — facts about domain entities (`approval(...)`, `chemical_reaction(...)`, `sales_order(...)`)
- **Metadata hyperedges** — facts about other hyperedges or entities (`type_def(...)`, `constraint(...)`, `index_declaration(...)`, `inference_rule(...)`)

The engine looks for metadata-shaped types when running Layer 2/3/4 logic. Otherwise it treats every hyperedge identically.

**Consequences (what uniform primitives unlock):**

- Query metadata with the same DSL as data — no separate INFORMATION_SCHEMA
- MVCC applies to schema changes — time-travel for "what schema did we have last year?"
- Retention policies apply to metadata — keep schema evolution audit forever, or prune
- Metadata can describe metadata recursively — a constraint can constrain another constraint
- Apps introduce their own metadata kinds without engine changes

### 6.8 What this solves — problems traditional schema cannot

Each scenario below names a real pain point in traditional SQL / DDL schema and shows how nDB's same-primitive design solves it natively. These are not "nicer to do in nDB" — most are genuinely impossible or require heroic workarounds in traditional schema.

**1. Time-traveling schema (regulatory compliance)**

*Traditional:* Schema migrations destructively replace the old schema. The new schema overwrites the old; the old schema only lives in migration files, not the database itself. To query "what was the Account schema in 2023?" you would restore a backup. Querying historical data under historical schema is impossible without that restore.

*nDB:* Metadata is MVCC-versioned alongside data.

```
as of 2023-12-31
match type_def(name: "Account", properties: ?p)
return ?p
```

Same query DSL. No backup required. Critical for Vietnamese accounting's transition from TT200/2014 to TT99/2025 — auditors in 2030 will need to interpret 2024 data under TT200 schema.

**2. AI-emergent schemas (LLM workloads)**

*Traditional:* Impossible. Schema must be declared BEFORE data exists. An LLM extracting structured facts from documents cannot extend a SQL schema mid-ingest without DBA intervention.

*nDB:* The LLM agent writes `type_def` hyperedges as it discovers concepts. Schema EMERGES from data. No migration step, no DBA in the loop, no `ALTER TABLE`.

```
# After ingesting 5 papers
write type_def(name: "clinical_trial", confidence: 0.6)

# After ingesting 500 more, the agent firms up the type
write constraint(target_type: "clinical_trial",
                 rule_expr: "phase >= 1 AND phase <= 4",
                 severity: "soft")
```

**3. Multi-tenant schema variations**

*Traditional:* Two bad choices — separate database per tenant (heavy, expensive, hard to update) OR shared schema with optional NULL columns (every tenant pays storage cost for every other tenant's customization; access control becomes painful).

*nDB:* Each tenant is an entity. Other entities link to a tenant via `belongs_to`. Tenant-specific type definitions are disambiguated by the tenant link: `type_def(name: "Customer", belongs_to: tenant_acme, ...)` is a different hyperedge from `type_def(name: "Customer", belongs_to: tenant_xyz, ...)`. Queries scope by tenant via the same `belongs_to` filter. Apps needing bulk-tenant admin operations (e.g. "delete everything tenant X owns") register a custom plugin index that maintains the tenant→entities mapping efficiently. Same engine, zero cross-tenant pollution, no namespace primitive needed.

**4. Domain-specific metadata extensions**

*Traditional:* Adding a new KIND of metadata (e.g., chemical reaction pathway definition) requires custom tables (more SQL schema), JSON columns (no validation), or a PostgreSQL EXTENSION (engine-level integration, hard to ship).

*nDB:* The chemistry app writes metadata hyperedges with types the engine doesn't recognize specially — `reaction_pathway_def`, `compound_classification_rule`, `safety_data_sheet_template`. The app's own code interprets them. No engine modification, no fork, no extension API.

**5. Schema audit (regulatory + governance)**

*Traditional:* DDL changes typically bypass the audit machinery applied to DML. Schema changes are logged to migration files (not the audit table) or require custom audit triggers per DDL command. "Who changed the Customer schema last week and why" is awkward and inconsistent.

*nDB:* Metadata writes carry the same provenance hyperedges as data writes. Schema changes flow through the same MVCC + retention + audit machinery as financial transactions. Standard query returns the full audit trail of every schema change.

**6. Self-describing data exports**

*Traditional:* An export is data without schema. Recipient gets a SQL dump or CSV plus a separate DDL file or OpenAPI spec. Validation requires loading both into matching tools. Schema drift between sender and recipient is a real risk.

*nDB:* Export includes data hyperedges AND metadata hyperedges in the same stream. Recipient queries the metadata to learn the structure, then queries the data — same DSL, no impedance mismatch, no separate validator.

**7. Schema as queryable knowledge**

*Traditional:* SQL's `INFORMATION_SCHEMA` exists but uses a separate query API and lives in a separate namespace. Joining schema metadata with business data is awkward. You can't easily run analytics over your own schema.

*nDB:* Schemas are first-class queryable data.

```
# Find every type that has a "currency" property
match type_def(name: ?t, properties: ?p)
where contains(?p, "currency")
return ?t
```

Same DSL as any data query. Useful for building generic UIs, generic search across heterogeneous domains, generic export tools.

**8. Federated schema reconciliation**

*Traditional:* Merging two databases with different schemas requires bespoke code to compare DDL, identify conflicts, write migration scripts to align them. Each merge is a custom project.

*nDB:* Schema reconciliation is a query problem. Query both metadata sets, find `type_def` conflicts, propose merges — all within the engine, using the same DSL, no separate schema-merging tool.

**9. Schema versioning without migrations**

*Traditional:* Versioning a schema means writing migration scripts (forward AND backward), testing them, running them, dealing with failed migrations on production. The schema can only be in one version at a time per database.

*nDB:* Multiple versions of a type can coexist as separate `type_def` hyperedges with different validity windows. Data tagged with which version applies. No global migration moment; new data uses new version, old data still validates against its own.

---

The common thread: traditional schema is a SEPARATE primitive with its own machinery (DDL, system catalog, migrations, INFORMATION_SCHEMA, audit triggers). nDB collapses that machinery into the data model itself — and every property of the data model (MVCC, retention, querying, audit) applies to schema for free.

---

## 7. Slicer Architecture

### 7.1 The slicer concept

A slicer is a **declarative projection** from the n-dimensional hyperedge graph onto a k-dimensional visual space. Slicers live in a **separate companion crate** (`nDB-slicer`), not in the engine. The engine knows nothing about slicers — it only provides the query execution machinery that slicers build on top of.

This means apps can use the built-in slicer crate, ship their own custom slicer crate, or skip slicers entirely if they only need raw query access.

```
slicer = Projection {
    select:    HyperEdgeSelector,        // which hyperedges to include
    project:   [DimensionMapping],       // n-D graph -> k-D visual
    aggregate: Option<Aggregator>,       // optional rollup
    encode:    VisualEncoding,           // table | scatter | sankey | 3D | ...
}
```

Different slicers over the same data produce different views. The data is stored once. Views are computed.

### 7.2 Theoretical grounding

This is the **grammar of graphics** (Wilkinson, 1999) applied to hypergraphs instead of tabular data. Implementations exist for tables (`ggplot2`, `Vega`, `Vega-Lite`); none exist for hypergraphs as a first-class engine feature.

Slicer presets follow **Mackinlay's ranking of visual variables** (1986): position > length > angle > area > color > shape — ordered by human perceptual accuracy.

### 7.3 Visual variable capacity

A single visualization can encode roughly 8 data dimensions before cognitive collapse:

| Visual variable          | Dimensions | Best for                  |
|---|---|---|
| Position (x, y)          | 2          | Continuous, comparable    |
| Position (z) / perspective | +1       | Continuous with depth     |
| Color hue                | +1         | Categorical (<= 7)        |
| Color saturation         | +1         | Ordered                   |
| Size                     | +1         | Continuous positive       |
| Shape                    | +1         | Categorical (<= 5)        |
| Motion / animation       | +1         | Categorical or temporal   |
| Opacity                  | +1         | Continuous                |

Tufte-school cartographers use this routinely; BI tools usually cap at 4-5.

### 7.4 Example: 5D slicer

A Sales Order hyperedge with 7 dimensions:
`{customer, item, amount, posting_date, salesperson, status, is_overdue}`

One slicer projects:

```
customer       -> x-axis      (categorical)
posting_date   -> y-axis      (continuous)
amount         -> color hue   (continuous)
status         -> shape       (categorical)
is_overdue     -> animation   (boolean pulse)
```

A different slicer over the same data could produce a pivot table, sankey diagram, 3D scatter, or network view. No data duplication.

### 7.5 Storage implication

For slicers to be interactive (sub-second), storage must support:

- Per-role attribute indexes (filter by customer fast)
- Columnar layout of hot dimensions (aggregate along amount fast)
- Streaming cursors (no full materialization for large slices)
- Pre-computed projections for hot patterns (materialized view equivalent)

This must be designed in from day 1, not retrofitted.

### 7.6 Compute responsibility: engine retrieves, slicer computes

**The engine does retrieval. The slicer does computation.** This is a load-bearing architectural principle.

| Operation | Lives in |
|---|---|
| Pattern matching, filtering, projection, time-travel, write, limit | Engine query language (Section 12) |
| Aggregation (`sum`, `count`, `avg`, `max`, `min`) | Slicer crate |
| Grouping (`group_by`) | Slicer crate |
| Having clauses (post-aggregation filter) | Slicer crate |
| Sorting (`order_by`) | Slicer crate |
| Math expressions (`a + b * c`) | Slicer crate |
| Currency conversion, formatting, business calculations | Slicer crate or app |
| Window functions | Slicer crate |
| Visual encoding (mapping dimensions to visual variables) | Slicer crate |

**Aggregation pushdown via index plugins.** Naive read: engine streams ALL rows to slicer for aggregation = bad for large datasets. The architectural answer: some index plugins (notably columnar — Section 13.2) expose aggregation as a plugin-specific API. The slicer detects when an aggregation can be served by an index and uses it; falls back to streaming when no index is available.

```
slicer asks: "SUM(amount) BY customer"
         |
         v
+-------------------------------+
| columnar index on amount      |    if registered:
| (handles aggregation natively)|    -> serve aggregation directly
+-------------------------------+
         else fallback:
         engine streams raw rows -> slicer aggregates in memory
```

**Slicer API sketch (Rust):**

```rust
let result = slicer
    .from_query(query)
    .group_by("cust")
    .aggregate(Sum::new("amt"), "total")
    .filter(|r| r.total > 10000)
    .sort_by_desc("total")
    .limit(100)
    .collect();
```

The slicer crate provides aggregation, grouping, math, sorting, and the projection API. Engine remains untouched.

**Slicer projection example (visual encoding):**

```
slicer "sales by customer over time"
  from_query:
    match
      sales_order(customer: ?cust, amount: ?amt, posting_date: ?dt)
  group_by: ?cust, ?dt
  aggregate: sum(?amt) as total_per_day
  project:
    ?cust         -> x_axis      (categorical)
    ?dt           -> y_axis      (continuous, daily)
    total_per_day -> color       (continuous)
```

The slicer combines query + aggregation + visual mapping into one declarative artifact. The engine only sees the underlying `match` query.

### 7.7 Deployment topologies: where you put the slicer

The slicer-as-companion-crate design means apps choose deployment topology to fit their performance needs. All trade-offs have solutions within the architecture; the choice is operational, not architectural.

| Topology | Engine ↔ Slicer boundary | Latency | Best for |
|---|---|---|---|
| **All embedded** (app + slicer + engine in one process) | Function calls | Nanoseconds | Fast applications, embedded analytics, CLI tools, desktop apps |
| **Slicer embedded, engine local server** (slicer in app, engine on same machine) | Unix socket / loopback HTTP | Tens of microseconds | Most production server apps; engine isolation without big perf hit |
| **Slicer embedded, engine remote** | Cross-network HTTP/JSONL | Single-digit ms + bandwidth | Distributed apps with central engine; works for AI/BI workloads |
| **All separate** (app talks to slicer service talks to engine) | Two network hops | 10s of ms | Multi-tenant SaaS where slicer service is shared; rare for latency-sensitive apps |

A single nDB deployment can use multiple topologies for different concerns. An app might embed the slicer for its hot-path queries while running a separate slicer service for batch jobs that talk to the same engine.

**This is the answer to the performance question:** trade-offs that look like cons in one topology disappear in another. Apps pick the topology that matches their latency / scalability / operational needs.

---

## 8. Identifier Strategy

### 8.1 Two-layer identifier system

**Internal: opaque UUID v7.**
- 128 bits, fixed width, fast comparison, mmap-friendly
- Time-ordered (newer entities cluster on disk for index friendliness)
- No coordination needed for generation (distribution-ready)
- Forever-stable — renaming/moving an entity never changes its internal ID

**External: pluggable lookup keys.**
- Apps declare which attributes serve as lookup keys per entity type
  (e.g. `customer_code`, `email`, `tax_id`)
- Multiple lookup keys per entity allowed
- Reverse index maintained from lookup-key value to internal UUID
- Lookup keys can change; internal UUID stays

### 8.2 Why two layers

| Property                | Internal UUID | External lookup |
|---|---|---|
| Performance             | Fast (fixed width) | Slower (string compare) |
| Human-readable          | No            | Yes             |
| Stable across renames   | Yes           | No              |
| Federation-safe         | Yes           | No (per-app)    |
| URL-friendly            | No            | Yes             |
| Required for storage    | Yes           | No              |

Hyperedges store references to internal UUIDs only. Lookup keys are an index, not a primary identifier. This is the pattern used by Wikidata, Datomic, and Frappe — proven in production.

### 8.3 IRIs and paths

IRIs and hierarchical paths are **computed on demand** from lookup keys for export, federation, and URL routing. They are not the primary identifier and not stored in hyperedges.

### 8.4 Renames are cheap

Changing an entity's external lookup-key value (e.g. "ACME" becomes "ACME Corp Ltd") updates the index only. No hyperedges are rewritten. No references break.

---

## 9. Persistence Model and Retention

### 9.1 Decision: append-only storage core

**Decided.** The storage core is append-only. The engine stores assertions immutably. Updates are new assertions superseding older ones. Deletions are tombstone assertions. The current state of any entity or hyperedge is the most recent assertion about it.

Rationale — append-only cascades into wins for all three target applications:

- **Provenance is free** — the history *is* the storage; no separate audit tables required
- **MVCC is natural** — every write already creates a new version
- **LSM-friendly** — physical storage maps cleanly to log-structured merge trees
- **Time-as-dimension** — "as-of time T" queries become a feature, not a project
- **GDPR-compatible** — selective deletion via compaction is supported

### 9.2 The storage bloat concern, addressed

Naive append-only databases grow without bound. nDB addresses this with five layered mechanisms:

1. **Compaction** — automatic, LSM-driven. Old versions no transaction needs get dropped during background merges.
2. **Retention policies** — configurable per type and per attribute (see 9.3).
3. **Hot/cold tiering** — recent data on fast SSD, older immutable history on cheap cold storage (S3, archival).
4. **Compression** — block-level, columnar where possible. Expected 5–10x ratios on structured data.
5. **Selective versioning** — apps mark attributes as audited, versioned, or latest-only.

Realistic numbers (large ERP, 100M facts/year, 5-year retention): ~100 GB hot, ~$2/month cold archival. Roughly 3x larger than equivalent update-in-place, but tiny by modern standards — and includes audit trail, MVCC, and provenance that update-in-place would need to bolt on.

### 9.3 Retention policy model

Retention is a first-class policy layer, declared during schema setup, enforced during compaction.

```
RetentionPolicy {
    versioning:       Audited | Versioned(Duration) | LatestOnly
    cold_tier_after:  Option<Duration>   // demote to cold storage after
    forget_after:     Option<Duration>   // physically delete after
}
```

Versioning modes:

- **Audited** — full append-only history preserved (until `forget_after`, if set)
- **Versioned(N)** — history kept for duration N, then old versions compacted away
- **LatestOnly** — overwrite-equivalent semantics; storage retains only the latest assertion

Typical configurations:

| Data type                     | Versioning      | Cold after | Forget after |
|---|---|---|---|
| Financial assertions          | Audited         | 1 year     | 10 years     |
| Status / ownership changes    | Versioned(2y)   | 6 months   | 2 years      |
| User profile attributes       | Versioned(3y)   | 1 year     | 3 years      |
| Operational logs / cache      | LatestOnly      | —          | —            |
| AI extracted facts            | Versioned(1y)   | 3 months   | 1 year       |
| Provenance hyperedges         | Audited         | 2 years    | never        |

### 9.4 Implications for the storage core

- Storage core is unconditionally append-only — never updates in place
- Compaction is the only path to physical deletion
- Retention policies drive compaction decisions, not application code
- "Forget after N" supports GDPR-style right-to-be-forgotten
- Once compacted away, historical versions are truly gone (irreversible)

### 9.5 Trade-offs accepted

- ~3x storage footprint vs comparable update-in-place engine (acceptable given modern storage costs)
- Compaction adds background CPU/IO overhead (configurable, scheduled)
- Cold-tier queries are slower (acceptable for old data; query planner routes to hot tier when possible)
- "Forget" operations are irreversible once compaction runs
- Retention policy configuration adds setup complexity (mitigated by sensible defaults per entity type)

---

## 10. Transaction Model

### 10.1 Decision: MVCC

**Decided.** nDB uses Multi-Version Concurrency Control (MVCC). Each transaction sees a consistent snapshot of the database as of its start time. Writers create new versions instead of overwriting. No reader blocks a writer, and no writer blocks a reader on read-write conflicts.

MVCC is the natural fit with the append-only storage core (Section 9). Because append-only already keeps every version, MVCC requires no separate version-tracking machinery — only visibility logic at read time and snapshot-aware compaction.

### 10.2 Isolation levels (per transaction)

Two isolation levels supported. Caller specifies isolation level when starting a transaction. Optional per-type default can be declared in the `type_def` for convenience.

- **Snapshot Isolation (SI)** — default for AI / analytics / read-heavy workloads. Each transaction sees its consistent snapshot. Highest throughput. Rare write-skew anomalies possible (two transactions each see consistent snapshots but their combined writes produce an inconsistent state).
- **Serializable Snapshot Isolation (SSI)** — default for ERP / financial workloads. SI plus conflict detection that aborts transactions which would produce write-skew. Slightly higher overhead, no anomalies. Pattern proven in PostgreSQL 9.1+ and CockroachDB.

Different transactions in the same database can run at different isolation levels. An app processing both financial JEs (SSI) and AI-extracted facts (SI) can mix them freely.

### 10.3 What MVCC enables

- **Long-running reads don't block writes.** ERP reports scanning millions of facts run concurrently with daily transaction posting.
- **Time-travel queries (as-of-T).** "Show me the database state at 2025-12-31" is a snapshot-ID lookup, not a project.
- **Audit queries non-blocking.** Reading the full modification history of an entity does not lock anyone out.
- **Batch jobs isolated.** Long-running AI extraction or migration jobs run without affecting concurrent operational writes.

### 10.4 What MVCC requires

- **Transaction IDs / snapshot IDs** — every assertion gets a transaction ID; every read transaction gets a snapshot reference.
- **Visibility logic** — when reading, determine which version of each entity is visible to the current snapshot.
- **Snapshot-aware compaction** — old versions cannot be removed while any active transaction needs them. Garbage collection waits for the oldest live snapshot to advance.
- **Conflict detection (SSI transactions)** — track read sets and write sets per transaction; abort transactions that would create cycles in the precedence graph.

### 10.5 Trade-offs accepted

- Write skew possible under plain SI (mitigated by SSI for transactions that need it)
- Long-running read transactions delay compaction (mitigated by snapshot timeouts + retention policies)
- Increased transient storage during heavy write activity (versions accumulate before compaction catches up)
- Snapshot ID overhead per assertion (~8 bytes, marginal)
- SSI conflict detection adds CPU cost for high-contention workloads

---

## 11. Primary Storage Format

The canonical record format — how the append-only hyperedge log + entity record store live on disk. Index layout choices are a separate question (see Section 13.2); this is purely about ground-truth storage.

### 11.1 File format: custom binary

**Decided.**

Rationale:
- Variable-arity hyperedges encode awkwardly in any schema-on-write format (Protobuf, Arrow, FlatBuffers, Cap'n Proto)
- MVCC fields (assertion tx ID, supersession tx ID) embed cleanly with custom layout
- LSM-style append + compaction wants tight control over record boundaries and block layout
- Zero-copy reads achievable with careful layout (don't need a framework for this)
- We can use standard crates for low-level pieces (CRC32, Zstd/LZ4 compression, varint encoding) without committing to a heavyweight serialization framework
- No external library version risk in the durability path

Rejected alternatives:
- **Apache Arrow** — columnar, designed for analytics. Better fit for the index tier (see 13.2), not primary. Variable-arity records don't fit cleanly.
- **FlatBuffers / Cap'n Proto** — zero-copy is appealing, but schema-on-write clashes with hyperedge variable arity and MVCC field embedding.
- **Protobuf** — too slow on the hot path; not zero-copy.

### 11.2 Record layout

**Decided.** All multi-byte fields little-endian throughout.

Six record kinds in primary storage:

| kind | id | purpose |
|---|---|---|
| 0x01 | EntityRecord | entity assertion (id + properties + MVCC) |
| 0x02 | HyperEdgeRecord | hyperedge assertion (id + type + roles + properties + MVCC) |
| 0x03 | TombstoneRecord | explicit deletion marker |
| 0x04 | TypeDefRecord | type-name dictionary entry |
| 0x05 | RoleDefRecord | role-name dictionary entry |
| 0x06 | PropertyDefRecord | property-key dictionary entry |

Dictionaries are themselves stored as records — no special file format or out-of-band metadata.

**HyperEdgeRecord:**

```
record_size: u32                                  (4 bytes)
record_kind: u8 = 0x02                            (1 byte)
version: u8                                       (1 byte)
hyperedge_id: UUID v7                             (16 bytes)
type_id: u32 (dictionary reference)               (4 bytes)
tx_id_assert: u64                                 (8 bytes)
tx_id_supersede: u64 (= u64::MAX when active)     (8 bytes)
arity: u8                                         (1 byte)
roles: [(role_id: u32, entity_id: UUID)] * arity  (20 bytes each)
property_count: u16                               (2 bytes)
properties: [(prop_id: u32, value: Value)] * cnt  (variable)
crc32: u32                                        (4 bytes)
```

Fixed overhead 49 bytes. Per role 20 bytes. Per property 4 + `value_size` bytes.

**EntityRecord:**

```
record_size: u32                                  (4)
record_kind: u8 = 0x01                            (1)
version: u8                                       (1)
entity_id: UUID v7                                (16)
type_id: u32 (0 = untyped)                        (4)
tx_id_assert: u64                                 (8)
tx_id_supersede: u64                              (8)
property_count: u16                               (2)
properties: [(prop_id: u32, value: Value)] * cnt  (variable)
crc32: u32                                        (4)
```

Fixed overhead 48 bytes.

**TombstoneRecord:**

```
record_size: u32, record_kind: u8 = 0x03, version: u8,
target_id: UUID v7, tx_id_supersede: u64, crc32: u32
```

Total 34 bytes.

**Value (tagged union for property values):**

```
tag  payload
---  -------------------------------------------------
0x01 (none — null)
0x02 bool (1 byte)
0x03 i64 (8 bytes)
0x04 f64 (8 bytes)
0x05 string: u32 length + UTF-8 bytes
0x06 bytes:  u32 length + raw
0x07 timestamp: i64 microseconds since Unix epoch
0x08 UUID v7 (16 bytes) — entity reference
0x09 decimal: u8 scale + i128 mantissa (16 bytes)
0x0A vector: u32 length + f32 array (for embeddings)
0xFF extension: u32 length + arbitrary bytes (future-proofing)
```

### 11.3 Design decisions baked in

- **Dictionary encoding** for `type` / `role_name` / `property_key`. Each unique name gets a u32 ID via a `*DefRecord`. Saves ~10-25 bytes per occurrence vs inline strings; makes index comparisons faster.
- **`u64::MAX` sentinel** for active supersession (instead of `Option<u64>`). Saves 1 byte per record × billions of records. Same trick PostgreSQL uses for `xmax = 0`.
- **Self-describing values** (tagged union). Fits schemaless storage core (Section 6 Layer 1). Schema can be added/changed without rewriting records. Same property can hold different types in different records.
- **CRC32 per record** for corruption detection. Covers everything from `record_size` through last payload byte.
- **`record_size` first** so corrupted records can be skipped during scan recovery.
- **Six explicit record kinds** instead of one polymorphic format — keeps parser simple and lets compaction handle each kind correctly.

Typical record sizes: 5-arity approval hyperedge with 1 short string property ≈ 180 bytes uncompressed. 100M hyperedges/year ≈ 18 GB raw, ~4-6 GB after Zstd block compression.

### 11.4 Open sub-questions

- **Block size + alignment** — page-aligned for mmap (4KB / 16KB), or stream-style with variable blocks?
- **SSTable sort key** — by entity ID? By hyperedge ID? By transaction ID? Multiple orderings via separate files?
- **WAL strategy** — separate write-ahead log, or LSM memtable acts as WAL?
- **mmap vs explicit buffer pool** — Rust's memory model affects this
- **Crash recovery** — checksum strategy at file/block level (record-level CRC already decided), partial-write detection, replay logic
- **Compression** — Zstd or LZ4 for block compression; what compression level / block size?

These belong in a focused Storage Implementation Spec rather than this architectural doc.

---

## 12. Query Language

**Decided.** Three coupled decisions.

### 12.1 Paradigm: declarative pattern matching (Datalog-influenced)

Pattern matching handles n-ary hyperedges natively; traversal languages (Cypher, Gremlin) are binary-edge-shaped and would fight us forever. Pattern matching is also algebraic and composable.

### 12.2 Wire format: structured AST

JSON or MessagePack. The optimizer consumes the AST, not raw text. LLMs and programmatic clients produce it directly without going through a text parser. Surface syntaxes compile down to it. Future-proofs the engine — alternative surface syntaxes (TypeQL-like, Cypher-like) can be added later without changing the engine.

### 12.3 Surface syntax: SQL-like keywords + hyperedge pattern primitives

Not LISP parens (alienates most devs), not Cypher ASCII art (binary-shaped). Familiar shell + hyperedge-native primitives.

### 12.4 Optional Rust embedded DSL

For compile-time type-safe queries from Rust code, compiling to the same wire format.

### 12.5 Rejected alternatives

- **Cypher / Gremlin traversal** — `()-[]->()` syntax is binary-shaped; extending awkwardly defeats the familiarity benefit
- **SPARQL** — triple-oriented; n-ary requires reification, which is exactly what we're avoiding
- **SQL-only** — forces tabular mindset over a graph data model
- **TypeQL** — viable but requires schema-strict (we want schemaless core)
- **Pure embedded Rust DSL** — locks out non-Rust clients
- **GraphQL** — read-mostly, doesn't compose joins

### 12.6 What the engine query language does (and does NOT)

**The engine query language is retrieval-only.** It supports pattern matching, filtering, projection, limits, time-travel, and writes. It does NOT include aggregation (`sum`, `count`, `avg`), grouping (`group by`), having clauses, sorting (`order by`), math expressions, or window functions.

Those operations live in the **slicer crate** (Section 7), not in the engine. The engine retrieves matching rows; the slicer computes on them. Some index plugins (notably columnar — Section 13.2) may expose aggregation as a plugin-specific API that the slicer uses opportunistically; this keeps aggregation fast when an aggregation-capable index exists.

This split is intentional. It keeps the engine minimal, makes the wire protocol simple, and lets apps use any compute library of their choice via the slicer crate.

Engine query language examples:

```
# Basic pattern match
match
  approval(document: ?doc, approver: ?alice, workflow: "fast-track")
return ?doc, ?alice

# Joining patterns + filtering
match
  sales_order(customer: ?cust, amount: ?amt, posting_date: ?dt)
  customer(id: ?cust, name: ?name, region: "Vietnam")
where ?amt > 1000
return ?name, ?amt, ?dt
limit 1000

# Multi-participant pattern (no arrow syntax — pattern joins)
match
  approval(document: ?doc, approver: ?alice)
  approval(document: ?doc, approver: ?bob)
where ?alice != ?bob
return ?doc, ?alice, ?bob

# Medical diagnostic reasoning — cross-reference multi-dimensional pathways
match
  diagnosis(patient: ?p, symptom: "fever", pathogen: ?disease)
  diagnosis(patient: ?p, symptom: "rash", pathogen: ?disease)
  treatment(disease: ?disease, medication: ?med, contraindication: ?allergen)
  patient_record(id: ?p, known_allergy: ?allergen)
return ?p, ?med, ?allergen
# Returns patients whose recommended treatment conflicts with a known allergy

# Biochemistry — find reactions producing a target compound under a condition
match
  chemical_reaction(product: ?product, catalyst: ?cat, temperature: ?temp)
where ?product = SodiumChloride and ?temp < T_50C
return ?cat, ?temp

# Time travel (free from MVCC + append-only)
as of 2025-12-31
match
  customer(id: ?cust, balance: ?bal)
return ?cust, ?bal
```

For aggregation, sorting, grouping, slicer projection, and visual encoding, see Section 7.

### 12.7 Wire format example

The first query above compiles to:

```json
{
  "kind": "query",
  "match": [
    {"kind": "hyperedge_pattern", "type": "approval",
     "roles": {"document":  {"var": "doc"},
               "approver":  {"var": "alice"},
               "workflow":  {"literal": "fast-track"}}}
  ],
  "return": [{"var": "doc"}, {"var": "alice"}]
}
```

LLMs produce this directly. Programmatic clients (Python, JS, Rust) build it via libraries without writing text. Tools serialize/deserialize easily.

### 12.8 Rust embedded DSL sketch (optional, type-safe)

```rust
let results = db.query()
    .match_pattern(Approval::pattern()
        .document(Var("doc"))
        .approver(Var("alice"))
        .workflow(Lit("fast-track")))
    .returning(vars!("doc", "alice"))
    .execute(snapshot)?;
```

Compiles to the same wire format.

### 12.9 Open sub-questions

Deferred to a focused query-language spec:

- Full grammar specification (BNF / EBNF)
- Operator precedence
- Subquery / CTE syntax (read pipelines, not aggregation)
- Recursive queries (path of unbounded length) — Datalog allows this; do we?
- Error message design
- Index hints / planner directives

---

## 13. Open Architectural Questions

These remain genuinely open. Each warrants its own focused spec.

### 13.1 Distribution

Candidates:
- **Single-node first** — start here, no distribution complexity
- **Read replicas** — cheap scaling, eventually consistent reads
- **Sharded by entity/edge** — hard, traversal queries become distributed
- **Replicated state machine** (Raft) — full distributed ACID, multi-year effort

Single-node for the first 2 years of work. Distribution is a separate architectural epoch.

### 13.2 Index strategy

**Decided: index framework with pluggable implementations.** The engine exposes a stable `Index` trait. All indexes — including built-ins — implement this trait. Some ship in core; others load as in-tree plugins or out-of-tree extensions.

Indexes are derived structures, rebuildable from primary storage. Multiple coexist over the same data. Indexes are registered per type (most common) or globally; the mix of registered indexes determines query performance characteristics.

**Index trait shape (sketch — final shape in a dedicated index spec):**

```
trait Index {
    fn name(&self) -> &str;
    fn on_insert(&mut self, record: &Record, tx_id: u64);
    fn on_supersede(&mut self, record: &Record, tx_id: u64);
    fn query(&self, predicate: &Predicate, snapshot: SnapshotId) -> Cursor;
    fn cost_estimate(&self, predicate: &Predicate) -> Cost;
    fn compact(&mut self, retention: &RetentionPolicy);
    fn rebuild_from(&mut self, primary_log: &PrimaryLog);
}
```

**Four layers can drive index creation:**

| Layer | How indexes are created | Example |
|---|---|---|
| Engine core | Hardcoded mandatory built-ins | Entity-by-ID always exists |
| Schema | Declarative: schema marks property as indexed | `Customer.email` → B-tree |
| Slicer | Materialized view derived from hot query patterns | Recurring slicer projection becomes pre-computed |
| App / extension | Plugin trait registration | Chemistry app registers structural-similarity index |

**Indexes by version** (see Section 16 Roadmap for full version scope):

| Index | Purpose | v1 | v2 | v3 |
|---|---|---|---|---|
| Entity-by-ID | Primary key lookup | mandatory built-in | — | — |
| Hyperedge-by-ID | Primary key lookup | mandatory built-in | — | — |
| Lookup-key reverse | External ID resolution (Section 8) | mandatory built-in | — | — |
| Adjacency list per entity | Graph traversal | mandatory built-in | — | — |
| Hyperedge-type clustering | Type filtering | mandatory built-in | — | — |
| Schema-declarative B-tree on property | Property filtering | built-in, schema-driven | — | — |
| Columnar per role | Slicer aggregation | — | in-tree plugin | — |
| Slicer materialized view | Hot slicer patterns | — | in-tree plugin (slicer-driven) | — |
| Full-text (Tantivy wrapper) | String search | — | opt-in per type | — |
| Vector (HNSW or IVF) | Embedding similarity | — | opt-in per type (decision in v2 spec) | — |
| Custom community plugins | Domain-specific | — | — | open plugin ecosystem |

**Why this architecture wins over a fixed index set:**

- V1 ships fewer indexes well rather than 10 indexes shallowly
- Extensibility is a feature, not a hack — published trait attracts contributors
- Slicer layer can request its own materialized views directly via the framework
- Future-proof: vector index research changes fast; we swap implementations without breaking the core
- Schema-driven indexing keeps configuration declarative (schema says "index this property" → engine creates B-tree)

**Trade-offs accepted:**

- Designing the plugin trait is itself architectural work (~2-4 weeks before any index is implemented)
- Plugin index quality varies; need validation / sandboxing strategy
- Query planner must accommodate future plugins from day 1 (designed to consume cost estimates from any registered index)

**Constraints from prior decisions:**

- All indexes must be rebuildable from primary storage (Section 11)
- All indexes must respect MVCC snapshot visibility (Section 10)
- Compaction in primary triggers incremental index update via `on_supersede`

**Plugin-specific query APIs (aggregation pushdown).** Beyond the base `Index` trait, plugins may expose specialized query interfaces. Notably, the columnar index plugin may expose aggregation (`sum`, `count`, `avg`, `min`, `max`, `group_by`) as plugin-specific methods. The slicer crate detects when a query's aggregation can be served by a registered plugin and routes it there; otherwise falls back to streaming raw rows from the engine and aggregating in the slicer's memory. This preserves the "engine retrieves, slicer computes" principle (Section 7.6) while keeping aggregation-heavy workloads fast.

**Open sub-questions** (deferred to focused index spec):

- Exact plugin trait signature (parameters, lifecycle hooks, async vs sync)
- Index sandboxing / failure isolation across plugins
- Query planner cost model details (how cost estimates compose)
- Whether each plugin manages its own physical storage or shares engine-provided buckets

### 13.3 Concurrency model

Open: thread-per-connection, async runtime (Tokio), green threads, work-stealing. Default leaning is async + Tokio given Rust ecosystem maturity, but decision deferred until storage layout is settled (concurrency model is tightly coupled to lock granularity in the storage core).

### 13.4 Error handling

Engine-level concerns to specify:
- Schema-violation reporting (when validation is on)
- Write conflict reporting (MVCC retries, SSI aborts)
- Storage corruption detection and recovery
- Query-time error surfacing through slicer projections

### 13.5 Testing strategy

Open. Needs:
- Property-based testing for graph invariants
- Deterministic replay for transaction model
- Fuzz testing for query language and storage format
- Comparative benchmarks vs Neo4j, TypeDB, TerminusDB

---

## 14. Non-Goals

Explicit statements of what nDB is **not** trying to be.

- **Not a SQL replacement.** SQL is correct for high-volume tabular aggregation and rigid ledgers. nDB targets workloads where rigidity is the bottleneck.
- **Not an OLTP system for high-frequency trading.** Throughput optimization for million-TPS scenarios is out of scope.
- **Not a document store.** Documents (JSON blobs) are anti-pattern in nDB; the engine wants entities and hyperedges, not opaque payloads.
- **Not a search engine.** Full-text search may exist as a feature but is not the primary access pattern.
- **Not a streaming engine.** Real-time event processing is out of scope; nDB ingests events but doesn't process streams as a primary workload.
- **Not an ad-hoc OLAP engine without index preparation.** The engine-retrieves / slicer-computes split (Section 7.6) means aggregation-heavy workloads need either a columnar index plugin (Section 13.2) or a materialized view. Workloads that do unpredictable ad-hoc analytical queries over raw data and expect sub-second response without any index preparation should use DuckDB, Snowflake, BigQuery, or ClickHouse instead. nDB is honest about this — when an aggregation-capable index exists, performance is competitive; without one, it falls back to streaming + slicer-side aggregation, which is slower than purpose-built OLAP engines.

---

## 15. Prior Art and References

### 15.1 Foundational

- **Berge, Claude.** *Graphes et hypergraphes* (1970). Original hypergraph formalism.
- **Wilkinson, Leland.** *The Grammar of Graphics* (1999). Theoretical basis for slicer architecture.
- **Mackinlay, Jock.** "Automating the design of graphical presentations of relational information" (1986). Visual variable hierarchy.
- **Cahill, Michael J., et al.** "Serializable Isolation for Snapshot Databases" (SIGMOD 2008). SSI algorithm.

### 15.2 Existing hypergraph databases

- **TypeDB** (formerly Grakn, ~2017) — most production-ready hyperedge-native database. Strong precedent. Schema-strict only.
- **HyperGraphDB** (2007) — general-purpose embedded hypergraph DB in Java.
- **GraphBrain** — NLP-focused hyperedges for semantic frames.

### 15.3 Adjacent inspirations

- **Datomic** — opaque entity IDs, time as a dimension, MVCC, append-only log. Closest spiritual ancestor.
- **Wikidata** — Q-numbers + multilingual labels = opaque + lookup pattern.
- **TerminusDB** — Git-style versioning of RDF, RDF-star quoted triples.
- **Neo4j** — property graph mindshare leader; instructive for what to do and what to avoid.
- **RDF / OWL / SHACL** — layered schema model proven on triples.
- **PostgreSQL** — MVCC reference implementation, observability and tooling to learn from.
- **CockroachDB / FoundationDB** — Rust-adjacent transactional systems with SSI and distributed MVCC.
- **RocksDB / LevelDB** — LSM tree implementations to study for the storage layer.

### 15.4 ERP context

- **Frappe / ERPNext** — DocType pattern as a hybrid of strict schema and flexible custom fields.
- **TT99/2025** (Vietnamese accounting circular) — example of why VAS-specific reports cannot tolerate schemaless data.

---

## 16. Roadmap

Scope organized by version. Calendar dates intentionally absent — versions ship when their scope is complete and quality is acceptable. Each version is a meaningful release with success criteria attached.

### 16.1 v1.0 — Initial Production Release

Goal: usable single-node production engine for one workload (AI reasoning OR ERP — decided closer to launch based on pilot interest).

**Storage:**
- Custom binary primary storage with full record layout (Section 11.2)
- Append-only LSM with per-type retention policies (Audited / Versioned / LatestOnly)
- Hot/cold tiering operational
- Crash recovery validated

**Transactions:**
- MVCC with both Snapshot Isolation and Serializable Snapshot Isolation, selectable per transaction (with optional per-type default)
- Time-travel queries (`as of T`)

**Identifiers:**
- UUID v7 internal + pluggable external lookup keys

**Indexes (built-in mandatory):**
- Entity-by-ID, Hyperedge-by-ID, Lookup-key reverse
- Adjacency list per entity
- Hyperedge-type clustering
- Schema-declarative property indexes (B-tree)
- Index framework + plugin trait stable

**Query language:**
- Custom DSL with pattern matching (Section 12)
- Structured AST wire format
- Read + write transactions
- Optional Rust embedded DSL

**Schema:**
- Layer 1 (schemaless, always)
- Layer 2 (type assertions, opt-in per type)
- Strict-write and soft-read enforcement modes

**Companion crates shipped alongside the engine:**

- `nDB-slicer` v1 — projection API + common projections (filter, group-by, aggregate)
- `nDB-renderer` v1 — **2D dimension renderers**:
  - Table (rows + sortable/filterable columns)
  - 2D scatter (x, y position)
  - 2D pivot table (categorical x, y axes + value cell)
  - 2D bar / line / area charts (same encoding family)
- `nDB-client-rust` v1 — wire-protocol client for Rust apps
- Engine, slicer, and renderer ship as separate crates; apps depend on whichever they need

**Success criteria:**
- 1M hyperedges, sub-second traversal queries on commodity hardware
- One real-world pilot application running in production
- Comparative benchmark vs Neo4j published
- Documentation site live

### 16.2 v2.0 — Analytics + AI Workloads

Goal: viable for AI / GraphRAG workloads and slicer-heavy analytics.

**Indexes added (in-tree plugins):**
- Columnar per-role
- Slicer materialized views (declarative, incremental update)
- Full-text index (Tantivy wrapper, opt-in per type)
- Vector index (HNSW or IVF — decision in dedicated v2 spec)

**Schema:**
- Layer 3 (constraints + validation per type)

**Companion crates added in v2:**

- `nDB-renderer` v2 — **3D and 4D dimension renderers**:
  - 3D scatter (x, y, z position)
  - 4D scatter (3D + color hue)
  - Sankey (multi-stage flow)
  - Network / force-directed graph (renders the hyperedge structure directly)
  - Heatmap (categorical x, y + intensity)
- `nDB-slicer` v2 — slicer presets per entity / hyperedge type
- `nDB-client-python`, `nDB-client-js` — wire-protocol clients for AI / web ecosystems

**Operational:**
- LLM integration patterns documented (GraphRAG, agent context)
- Comparative benchmarks vs TypeDB + TerminusDB published

**Success criteria:**
- 100M hyperedges, sub-second slicer aggregation
- Two real-world pilot applications across different target domains
- Vector index integration validated with at least one AI workload
- Schema Layer 3 validated with an ERP pilot

### 16.3 v3.0 — Distribution + Ecosystem

Goal: differentiated from competitors, ready for broader adoption.

**Schema:**
- Layer 4 (ontology + inference)

**Distribution:**
- Read replicas (eventually consistent reads)
- Federation: linking multiple nDB instances via cross-reference resolution

**Companion crates added in v3:**

- `nDB-renderer` v3 — **5D and 6D dimension renderers**:
  - 5D scatter (3D + color + size)
  - 6D scatter (3D + color + size + shape)
  - Choropleth / point map (geographic encoding when applicable)
  - Treemap / sunburst (hierarchical)
- `nDB-client-go`, `nDB-client-java` — additional wire-protocol clients

**Ecosystem:**
- Public plugin API documented and stable
- Community-contributed index plugins (spatial, temporal-specific, domain-specific)
- Community slicer / renderer crates
- Provenance / lineage as queryable first-class feature

**Success criteria:**
- At least one community-contributed plugin in production
- Federation working across 2+ instances in a real deployment
- Plugin API documentation site

### 16.4 v4.0+ — Distributed + High-Dimensional Renderers (future)

Goal: web-scale write workloads + saturating the visual variable hierarchy.

**Companion crates added in v4+:**

- `nDB-renderer` v4 — **7D and 8D dimension renderers** (approaching the cognitive ceiling):
  - 7D scatter (3D + color + size + shape + motion / animation over time)
  - 8D scatter (7D + opacity)
- Beyond 8D, visualizations exceed the documented cognitive ceiling (Section 7.3). Higher-arity hyperedges should be projected via multiple complementary slicers (small multiples) rather than packed into a single visualization.

**Distribution scope (separate architectural epoch):**

- Sharding by entity / edge with cross-shard traversal
- Raft-replicated state machine for distributed ACID
- Multi-region deployment

The distribution portion will require a fresh design doc when approached. The high-dimensional renderers can be built on top of the existing engine + slicer crates without distribution.

---

## 17. Risks and Open Concerns

- **Query language fragmentation** — no existing standard fits cleanly. Inventing a new DSL is correct but raises adoption friction.
- **Mindshare and ecosystem** — competing with Neo4j's mature tooling is a multi-year uphill battle.
- **Algorithm gap** — classic graph algorithms need redefinition for hyperedges; this is research, not just engineering.
- **TypeDB precedent** — they tried hyperedge-native (schema-strict only) and adoption is slow. We must understand exactly why before assuming we'll do better.
- **Effort scale** — production-grade DB engine in Rust is a 100k+ LOC, multi-year commitment. Sustainability of solo effort is a real concern.
- **Append-only storage cost** — ~3x overhead is acceptable today but worth monitoring as data scales. Retention policy tuning is operational discipline, not a code feature.
- **SSI implementation complexity** — serializable snapshot isolation is notoriously tricky to implement correctly. Pattern is proven (PostgreSQL, CockroachDB) but requires care.

---

## 18. Next Steps

1. **Review this design doc** — confirm the architectural decisions captured here match intent.
2. **Decompose remaining items into focused specs** — storage implementation (Section 11.4 sub-questions), index strategy (Section 13.2), query language grammar (Section 12.9), distribution (Section 13.1).
3. **Study TypeDB deeply** — clone the repo, read the schema design and query language, understand why adoption is slow.
4. **Study Datomic, PostgreSQL MVCC, and RocksDB** — Datomic for the append-only + MVCC reference architecture; PostgreSQL for MVCC + SSI patterns; RocksDB for LSM implementation patterns.
5. **Prototype the storage core** — minimal Rust crate exercising hyperedge insert/read/traverse with the decided record layout (Section 11.2), append-only with MVCC from the start.
6. **Prototype the query parser** — exercising the wire format AST (Section 12.2) and the text-syntax parser (Section 12.3).

This doc covers the architectural foundation. Subsequent specs (one per Section 13 item, plus storage implementation and query grammar) will decompose into implementable milestones.

---

*End of design.*
