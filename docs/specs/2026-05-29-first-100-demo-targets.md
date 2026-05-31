# nDB — First 100 Demo Targets ("like OpenAlex")

Status: PROSPECT LIST (working draft, 2026-05-29)
Selection rule: an organization that **owns or stewards a large graph-shaped dataset** where an
n-dimensional explorer over *their own data* is an obvious, showable demo. Sorted by
**demo-ability now** (open/legal/graph-shaped, warm) → **rich-but-hard** (proprietary, slow, gated).
Most demo-able first because OpenAlex-style open-data orgs let us demo on REAL data with zero
NDA/procurement friction, produce a public artifact + logo, and several are also potential buyers.

Legend — **Heat**: 🟢 warm/open (demo on public data now) · 🟡 reachable, some friction · 🔴 rich but hard (NDA/procurement/secrecy).
**Data**: the graph they own. **Why**: why the explorer demos well for them.

> CAVEAT (read first): this is **orgs + the data they hold**, not 100 named humans with verified
> contacts. The real outbound work is turning each row into a named person (who runs their KG/data
> team) + a reason to care. Tiers 1–3 are demoable on public data *today*; tiers 4–6 require their
> data, so they're "after the public explorer creates pull." Don't cold-chase 🔴 pre-engine.

## Tier 1 — Open scholarly / science graphs (🟢 demo on real public data NOW; OpenAlex's peers)
1. **OpenAlex / OurResearch** — 209M works, 250M authors, CC0. The reference case; landing them = the logo.
2. **Semantic Scholar / Allen Institute for AI (AI2)** — 200M+ papers + citation graph + embeddings (SPECTER). API open.
3. **Crossref** — 150M+ DOIs + citation/reference graph + funder/affiliation. Membership org, open data.
4. **OpenCitations** — open citation index (COCI), RDF/SPARQL. Tiny team, open-science aligned, warm.
5. **SemOpenAlex** — 26B RDF triples over OpenAlex (Karlsruhe/KIT). Academic, would co-publish.
6. **DataCite** — DOIs for datasets + the dataset-citation graph. Open, membership.
7. **ORCID** — researcher identity graph (researcher↔works↔orgs). Nonprofit, open data.
8. **ROR (Research Organization Registry)** — institution graph, CC0. Small, open.
9. **dblp (Schloss Dagstuhl)** — CS bibliography + co-authorship graph. Open, German research-funded.
10. **Lens.org (Cambia)** — scholarly + patent linked graph. Freemium; patent tie = budget.
11. **PubMed / NCBI (NLM)** — 36M biomedical citations + MeSH ontology graph. US-gov open.
12. **Europe PMC (EMBL-EBI)** — life-science literature + text-mined entity links. Open.
13. **CORE (Open University UK)** — 290M+ open-access papers aggregation. Open, grant-funded.
14. **Internet Archive Scholar / general IA** — scholarly graph + the broader IA link graph. Mission-aligned.
15. **Microsoft Academic Graph successors / MAKG** — the MAG-derived KG community.
16. **Wikidata / Wikimedia (Deutschland)** — the graph every LLM reads; ~115M items, fully open. Huge showcase.
17. **DBpedia Association** — Wikipedia-derived KG, SPARQL. Open, would co-publish.
18. **Open Research Knowledge Graph (TIB Hannover)** — structured research-contributions graph. Open, research-funded.
19. **Wellcome Trust data team** — funder; funds open bibliometrics (already funds VOSviewer full-text). Budget + open.
20. **CWTS Leiden (VOSviewer authors)** — bibliometrics lab; the incumbent's home. Co-build or co-opt.

## Tier 2 — Open biomedical / life-science knowledge graphs (🟢 public data, science-aligned)
21. **Bioconductor / Bio2RDF community** — linked life-science datasets, RDF. Open.
22. **EMBL-EBI** (beyond Europe PMC) — UniProt, ChEMBL, Ensembl, Reactome — each a graph. Open, huge.
23. **UniProt consortium** — protein↔function↔interaction graph. Open.
24. **ChEMBL** — bioactive molecule ↔ target ↔ assay graph. Open. (Direct pharma-shape demo.)
25. **Reactome** — biological pathway graph. Open.
26. **STRING-DB (SIB / EMBL)** — protein-protein interaction networks. Open.
27. **Disease Ontology / Monarch Initiative** — disease↔gene↔phenotype graph. Open, NIH-funded.
28. **Human Cell Atlas / CZI (Chan Zuckerberg)** — single-cell + biomedical KG; CZI has MONEY + open mandate.
29. **Open Targets (EMBL-EBI + pharma consortium)** — target↔disease↔drug graph. Pharma-backed, open. ⭐ bridge to 🔴 pharma.
30. **AlphaFold DB / DeepMind-EMBL** — 200M protein structures. (You already have an AlphaFold demo — natural.)
31. **PDB (Protein Data Bank / RCSB)** — structure graph. Open, NSF-funded.
32. **NIH / NLM data science** — many biomedical graphs, open-science mandate.
33. **PubChem (NCBI)** — chemical↔bioassay graph. Open.
34. **DrugBank (U. Alberta)** — drug↔target↔interaction; freemium → commercial tie.
35. **Pistoia Alliance** — pharma pre-competitive data-standards consortium. ⭐ door to many pharma at once.

## Tier 3 — Open civic / cultural / entity graphs (🟢 public, visually compelling demos)
36. **GLEIF** — global Legal Entity Identifier graph (entity↔parent↔relationship). Open, transparency mandate.
37. **OpenCorporates** — world's largest open company graph. Mission = transparency; warm.
38. **OpenSanctions** — sanctions/PEP entity graph. Open, investigative-journalism aligned.
39. **OCCRP / ICIJ (Panama Papers people)** — investigative entity-link graphs. Aleph platform; n-D explorer = obvious fit.
40. **MusicBrainz / MetaBrainz** — artist↔release↔recording graph. Open, beloved, great visual demo.
41. **Discogs** — music release graph. Open-ish API.
42. **IMDb / OMDb (open mirrors)** — film↔person↔title graph. Visually iconic demo.
43. **Open Library / Internet Archive** — book↔author↔edition graph. Open.
44. **Wikidata-for-GLAM (museums)** — Europeana, cultural-heritage linked data. Grant-funded, loves viz.
45. **Europeana** — 50M+ cultural heritage objects, linked. EU-funded, open.
46. **OpenStreetMap (geo graph)** — the road/place network. Open, huge community.
47. **GTFS / transit feeds (transit agencies)** — transit network graphs. Open; a single agency = a crisp demo.
48. **Open Food Facts** — product↔ingredient↔additive graph. Open, mission-driven.
49. **GeoNames** — place hierarchy graph. Open.
50. **Software Heritage (Inria)** — 5B+ source files / 1B commits dependency-and-history graph. Open, mission. ⭐ huge + novel.
51. **Libraries.io / ecosyste.ms** — open-source package dependency graph. Open. (Dep graphs demo beautifully.)
52. **GitHub dependency graph / deps.dev (Google)** — package↔dependency↔vuln graph. Public.
53. **CVE / NVD (MITRE / NIST)** — vulnerability↔product↔CWE graph. US-gov open; security audience.
54. **MITRE ATT&CK / D3FEND** — adversary technique graph. Open; cyber audience has budget.
55. **CourtListener / Free Law Project** — legal citation graph (case↔cites↔case). Open, the "OpenAlex of law."
56. **Caselaw Access Project (Harvard)** — US case law citation graph. Open.

## Tier 4 — Patent / IP / competitive-intel (🟡 budget-rich, semi-open data, faster buyers)
57. **USPTO PatentsView** — open US patent citation + inventor + assignee graph. Open data, gov.
58. **EPO Open Patent Services** — European patent graph. Semi-open.
59. **Google Patents Public Datasets (BigQuery)** — patent graph, public. Demo-able now.
60. **Lens.org patent side** (also Tier1) — scholarly↔patent linkage. Budget.
61. **Dimensions (Digital Science)** — papers+patents+grants+clinical-trials linked. Commercial; partner-or-compete.
62. **PatSnap** — IP intelligence platform. Has budget, graph-shaped product.
63. **Clarivate (Web of Science / Derwent)** — citation + patent incumbent. Big, slow, but the whale.
64. **Questel / IFI Claims** — patent data vendors. Budget.
65. **CAS (Chemical Abstracts Service / ACS)** — chemistry + reaction graph. Commercial, deep pockets.
66. **Corporate IP / R&D-strategy teams** (generic) — buy patent landscapes today; explorer = upgrade.

## Tier 5 — Pharma / biotech proprietary KG teams (🔴 richest buyers, slow; AFTER public pull)
67. **BenevolentAI** — 1B+ relationship biomedical KG. The canonical proprietary-KG buyer.
68. **Recursion Pharmaceuticals** — huge phenomics + bio KG. Public co, data-forward.
69. **Insilico Medicine** — generative drug-discovery KG.
70. **Relation Therapeutics** — graph-ML drug discovery.
71. **Genesis Therapeutics / Iambic / Isomorphic Labs (Alphabet)** — graph+ML drug discovery. Isomorphic = DeepMind money.
72. **Novartis data science (NIBR)** — internal biomedical KG team.
73. **AstraZeneca AI/KG team** — known graph investment.
74. **Genentech / Roche computational sciences** — internal KGs.
75. **Pfizer / GSK / Merck / Sanofi / Bayer / Boehringer** — each has a KG/data-science group (6 rows: 75–80).
76. *(GSK)* 77. *(Merck)* 78. *(Sanofi)* 79. *(Bayer)* 80. *(Boehringer Ingelheim)*
81. **BioNTech / Moderna** — omics + data-science teams.
82. **Tempus / Flatiron (oncology data)** — clinical-genomic graphs. Budget.
83. **23andMe / genomics cohorts** — variant↔phenotype graphs.

## Tier 6 — Finance, security, industry (🔴 mandatory-recurring pain, deepest budgets, hardest reach)
84. **Quant/fraud teams at major banks** (JPMorgan, Goldman, HSBC...) — transaction-temporal + counterparty graphs. (rows 84–87)
85. *(Goldman Sachs)* 86. *(HSBC)* 87. *(a tier-2 / fintech: Stripe Radar, Wise, Revolut fraud)*
88. **Card networks (Visa / Mastercard) risk** — transaction graphs at scale.
89. **AML/KYC vendors (Chainalysis, Elliptic — crypto graphs)** — blockchain entity graphs. ⭐ crypto graph = vivid demo, well-funded.
90. **Ratings / market-intel (Moody's, S&P, Bloomberg)** — entity+relationship graphs. Budget.
91. **Palantir-adjacent gov/defense primes** — entity-link graphs. Hardest, richest, most secret.
92. **Telecom network/ops (AT&T, Vodafone, Deutsche Telekom)** — network topology + CDR graphs.
93. **Supply-chain risk (Interos, Everstream, Resilinc)** — supplier-dependency graphs. Post-COVID budget.
94. **Cyber threat-intel (Recorded Future, Mandiant/Google, CrowdStrike)** — threat-actor↔IOC graphs. Budget + graph-native.
95. **SIEM/identity-graph teams (Splunk, Microsoft Defender, Okta)** — identity+access graphs.
96. **E-commerce / recommendation graph teams (Amazon, Alibaba, Shopify)** — product↔user↔co-purchase.
97. **Social / content graph teams (Reddit, Pinterest, LinkedIn economic graph)** — LinkedIn's "economic graph" is literally this pitch.
98. **Energy/utility grid-topology teams** — the grid is a temporal graph.
99. **Logistics network teams (Maersk, DHL, Flexport)** — route/shipment graphs.
100. **Insurance fraud / claims-network teams (major insurers)** — claim↔entity↔provider rings.

## How to use this list
- **Demo NOW (pre-paid-product):** Tiers 1–3 (🟢). Build the explorer on THEIR public data, send them the link.
  Cost ~0, produces logos + co-publications + the "can I load my own data?" inbound. Several (CZI, Wellcome,
  Pistoia, Open Targets) are bridges to the 🔴 pharma money.
- **Warm-but-budget:** Tier 4 (🟡 patent/IP) — public-enough data to demo, real budget, faster cycles than pharma.
  Strongest *first-paid* candidates alongside open-science orgs.
- **After public pull:** Tiers 5–6 (🔴) — only chase once the explorer is public AND the engine serves (M1) AND
  slicer→Arrow is wired (so "load your own data" is real). Cold-chasing these pre-engine converts ~0.
- **The real next step is NOT this list — it's turning 10 rows into 10 named humans + a reason each cares**, and
  shipping the public explorer so the best leads self-identify (inbound > guessed outbound, pre-revenue).

## See also
- `docs/specs/2026-05-29-ndb-thesis-and-wedge.md` (the strategy this serves)
- memory: ndb-thesis-wedge, ndb-next-real-openalex-10g, ndb-2026-05-29-explorer-10g-test
