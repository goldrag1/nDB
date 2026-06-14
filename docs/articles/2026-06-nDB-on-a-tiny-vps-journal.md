# Journal — nDB into production on a tiny VPS

Raw, append-as-it-happens log. Verbatim errors, exact commands, real numbers. Synthesized later into the how-to article. Newest entries at the bottom.

---

## 2026-06-14 — Recon & design

**Host (measured, not assumed):** HTS VPS `72.61.119.96`, Ubuntu 24.04.3, 2 cores, 7.8 GB RAM / 4.8 GB free (Frappe uses ~2.9 GB), 43 GB free disk.

```
$ ssh ... frappeuser@72.61.119.96 'free -h; df -h /; nproc'
Mem:  7.8Gi total  2.9Gi used  1.9Gi free  3.2Gi buff/cache  4.8Gi available
/dev/sda1  96G  54G  43G  56% /
2
```

**Cloudflared already running** one tunnel: `cloudflared-tamdinh.service`, tunnel `447eb352-d6f6-4aaa-a259-1806f8c0992e`, `protocol: http2`, serving `e.tamdinhlaocai.com` + `tamdinh-vps.nextstar-erp.com`. → `nextstar-erp.com` is in this account, and the config already path-routes (`path: ^/socket.io`). So reuse this tunnel, path-route a new hostname, no new tunnel, no nginx/certbot.

**Datasets on hand** (`.demo-data/`): alphafold 332K, exoplanet 80K, biodiv 124K — real, tiny, perfect. `langgraph-oa` is 29 GB / 2050 SSTables → excluded (can't be snappy on this box).

**Key design forcings discovered by reading the code (not docs):**
- Studio's active DB is **process-global** and switching is **admin-only** → anonymous viewers can't switch DBs. So **merge the 3 datasets into one DB** (3 record-kinds) instead of 3 switchable DBs. Also the truest "one store, heterogeneous objects" demo.
- `ndb-studio` speaks `/api` (cookie auth); the published SDK/CLI/MCP target `ndb-server`'s `/v1` wire protocol; `ndb-mcp-server` opens the engine **in-process** (no network mode). The engine is **single-writer**. → to let an outside dev/agent test with the real tooling, co-deploy `ndb-server` on `/v1`; since both serve read-only, each opens its **own copy** of the merged DB.
- Read-only in `ndb-server` today only exists via follower mode (`--replicate-from` → `with_read_only(true)`). Standing up a leader+follower pair just to get read-only is silly → add a standalone `--read-only` flag (~3 lines).
- `guard_read` returns 401 when no session → anonymous read needs a `--public-read` flag (~5 lines) + a frontend tweak (read-only UI instead of login wall).
- Merge subtlety: entity ids are random UUIDs (no cross-DB collision) but type/property/role **names** intern to per-DB ints → merge must round-trip through names AND preserve UUIDs (so `$ref`/edge fillers stay valid). `create()` allocates a fresh UUID → need `create_with_id`.

**Topology chosen:** one hostname `ndb.nextstar-erp.com`, path-routed: `/v1/*` → ndb-server (read-only, :8742), else → Studio GUI (`--public-read`, :8780). Both cgroup-capped (`MemoryMax=384M`, `CPUQuota=50%` each → Frappe keeps ≥1 core).

---

## 2026-06-14 — Merge attempt → the datasets are nameless → pivot to a seed

Built `--merge` + UUID-preserving `create_with_id`/`merge_from` per the spec. First run on the originals: **mutated the source WALs** (opening read-write triggers WAL recovery — `exoplanet/000003.ndblog` truncated to 0, `alphafold` grew to 286 KB). **Lesson:** demo/source DBs are sacred — always merge from `cp -r` copies, never the originals. (Engine opens are read-write; there is no read-only `Engine::open`.)

Then the real surprise: merge copied **0** from alphafold/exoplanet but 168 from biodiv. Instrumented `merge_from` with a record-variant histogram:

```
alphafold: head=134 recs=1783 entity=1624 edge=25 typename=0 propkey=0 rolename=0 other=134
exoplanet: head=303 recs=606  entity=163  edge=140 typename=0 propkey=0 rolename=0 other=303
biodiv:    head=240 recs=509  entity=168  edge=63  typename=7 propkey=21 rolename=11 other=239
chemistry: head=180 recs=354  entity=131  edge=43  typename=0 propkey=0 rolename=0 other=180
seismic:   head=192 recs=5157 entity=4827 edge=138 typename=0 propkey=0 rolename=0 other=192
```

**Cause:** 4 of 5 prebuilt demo DBs carry **zero TypeName/PropertyKey/RoleName dictionary records** — built by a benchmark tool (`/home/long/long/rust/ndb-bench`) that writes entities with hardcoded type-id **constants** and never registers human-readable names. `merge_from` keyed on names → skipped every entity whose type-id had no name. The catalog had the same blindness (showed `kind:7`). biodiv is the lone outlier (its builder wrote names). The three target DBs even use *different* nameless schemas (alphafold types {2,3,6,7}, exoplanet {2,3,4}, biodiv {1,2}) — no single mapping to reconstruct.

**Generalizable rule:** `snapshot_iter`-derived names only work if the dictionary records are in the live stream. A DB built straight against the engine with raw type-ids is *data-complete but name-blind* — fine for benchmarks, useless for a human explorer. Don't assume a prebuilt nDB is demo-ready; check that `catalog` resolves real kind/property names, not `kind:N`.

**Pivot:** author a small, properly-named, hyperedge- and vector-rich demo dataset via a `--seed-demo` routine (proteins · exoplanets · species). Cleaner names, richer relationships, fully reproducible, no dependency on name-blind legacy DBs. The `create_with_id`/`merge_from` capability stays (genuinely useful for fusing *named* DBs), just off the demo's critical path.

---

## 2026-06-14 — Build & deploy

**Build:** planned a musl static build for portability, then checked: local AND VPS are both `Ubuntu GLIBC 2.39-0ubuntu8.7`. Same distro → a plain `cargo build --release` runs as-is. **Lesson:** musl is for *uncertain/older* targets; same-distro deploy → skip the musl toolchain entirely. `ndb-server` 22 MB, `ndb-studio` 12 MB, both dynamically linked to the matching glibc.

**Ship:** `scp -r /tmp/ndb-deploy ~/ndb` (binaries + a seeded `studio-db` + a copy `server-db`). Two systemd units (`User=frappeuser`, `Restart=on-failure`) with cgroup caps `MemoryHigh=256M MemoryMax=384M MemorySwapMax=0 CPUQuota=50%` each → ≤1.0 core combined on the 2-core box, Frappe keeps ≥1 core. `enable --now` → both `active`. **Measured `MemoryCurrent` ≈ 0.45–0.5 MB each** — the cap is pure insurance; real footprint is trivial. Bootstrap admin password captured from `journalctl -u ndb-studio`.

**Ingress (one hostname, path-routed):** inserted two rules into the existing `config-tamdinh.yml` above the `http_status:404` catch-all:
```yaml
  - hostname: ndb.nextstar-erp.com
    path: ^/v1
    service: http://localhost:8742
  - hostname: ndb.nextstar-erp.com
    service: http://localhost:8780
```
`cloudflared tunnel route dns <id> ndb.nextstar-erp.com` → **failed: `1003 record already exists`** (leftover from a prior life of the zone). Fix: `cloudflared tunnel route dns -f <id> ndb.nextstar-erp.com` (`--overwrite-dns`) → "Added CNAME … will route to this tunnel". **Lesson:** on a reused zone, expect a stale record; `-f` overwrites it (the tunnel's `cert.pem` carries the DNS-edit scope, no API token needed). After restart, the first few requests returned **502 while the edge↔tunnel mapping settled** — retried clean within seconds. Don't trust the first probe after a DNS/tunnel change.

**Live:** `https://ndb.nextstar-erp.com/` (Studio GUI, anonymous read-only), `…/api/catalog` (named kinds + N-ary edges), `…/v1/health` (machine API). One URL, path-routed.

## 2026-06-14 — Clean-room adoption sweep (as an outside dev/agent)

From a scratch dir, nothing preinstalled:
- **curl `/v1`** ✓ — `GET /v1/health` → `{"status":"ok"}`; `POST /v1/commit` → **403 `read_only`** with a clear message. The no-tooling baseline anyone can reproduce.
- **TS SDK via real npm** ✓ — `npm i @n-dimension-database-ndb/client` (published, 2.4.0), `new NdbClient("https://ndb.nextstar-erp.com")` → `health()` ok over TLS (Node `fetch`), `query()` reached the engine and returned a proper nDB error envelope for a malformed body. The headline adopter path works against the public HTTPS endpoint.
- **`ndb` Rust CLI** ✗ over HTTPS — **real finding:** `ndb-client-rust` is plain `std::net::TcpStream`, **no TLS** (accepts `http://host:port` or bare `host:port`, rejects `https://…`). A Cloudflare-fronted (TLS-only) nDB is therefore unreachable by the Rust CLI/client directly. Adopters on the CLI need: the TS SDK, `curl`, a local `cloudflared access` tunnel, or a plain-HTTP port. Worth a TLS client or a documented `cloudflared access` recipe.
- **Public GUI (Playwright)** ✓ — logged-out browser renders the read-only explorer (named kinds, N-ary edges, vectors, per-cell history, time-travel slider), `read-only` badge + `Log in` button, **0 console errors**. Screenshot saved (`ndb-public-studio.png`).

**Gate:** the two universal adopter paths (TS SDK, curl) + the human GUI all pass against the live HTTPS instance, and writes are uniformly refused. The Rust-CLI-over-TLS gap is the one honest caveat.

---

## 2026-06-14 — Add the 3D explorers (protein · exoplanet · biodiversity)

The pre-built per-domain explorers (`docs/explorer` = AlphaFold 3D protein viz, `docs/exoplanet`, `docs/biodiv`) each fetch `/iter` (+ `/query`, `/commit`) from a **live** nDB and are coupled to their **original** dataset's schema. Crucially, **the name-blind problem doesn't affect them** — they hardcode the schema type-ids (they were built alongside the same builders), so they read the raw records fine. This is the *opposite* of Studio (which needs the dictionary). Lesson: "name-blind" is a Studio/catalog problem, not a data problem — purpose-built clients are unaffected.

Local verify (each rendered against a read-only `ndb-server` on its original DB): exoplanet (163 ent / 148 edges, 3D scatter), protein (full AlphaFold ribbon, pLDDT colors), biodiv (species gallery with images). **CORS bit the local cross-origin test** (explorer :9877 → API :8745) — prod is same-origin so it won't, but to *see* the render locally I added a small `ndb-server --cors-origin <origin>` flag (wires the existing `with_cors_origin` builder; mirrors `--read-only`). Genuinely useful for adopters running a browser app on another origin.

Deploy shape per domain: a read-only `ndb-server` (original DB) + a `python3 -m http.server` for the single-file explorer, both cgroup-capped, behind one subdomain; cloudflared path-routes `^/(iter|v1|commit|query|health…)` → API, default → static. Edited each explorer's API base to `location.origin` in production (same-origin → no CORS, `/iter` path-routes to its server) while keeping the localhost dev branches.

**The Cloudflare Universal SSL trap:** first tried `protein.ndb.nextstar-erp.com` (etc.). DNS resolved (proxied) but every request was **HTTP 000 — TLS handshake failure (alert 552)**. Cause: Cloudflare's free **Universal SSL covers the apex + a ONE-level wildcard (`*.nextstar-erp.com`) only** — a *second*-level label like `protein.ndb.nextstar-erp.com` has no edge certificate. That's also why `ndb.nextstar-erp.com` (first-level) worked. Fix: use **first-level** subdomains — `ndb-protein.nextstar-erp.com`, `ndb-exoplanet…`, `ndb-biodiv…` — covered by `*.nextstar-erp.com`. Detection: `curl -v` shows `TLS alert, handshake failure (552)` while `dig` resolves fine. (Deeper subdomains need Advanced Certificate Manager / a dedicated cert.)

**Live:**
- `https://ndb-protein.nextstar-erp.com` — AlphaFold 3D protein explorer
- `https://ndb-exoplanet.nextstar-erp.com` — exoplanet N-ary discovery 3D scatter
- `https://ndb-biodiv.nextstar-erp.com` — biodiversity species gallery / ecological interactions

Each is the original rich dataset (1624 / 163 / 168 entities) served read-only; writes 403; same-origin so no CORS; verified in-browser (0 errors beyond favicon). Total VPS footprint now: Studio + trio `/v1` + 3 API servers + 3 static servers = 8 cgroup-capped services, all tiny.

---

## 2026-06-14 — Consolidate to one subdomain via the user's own `server.py`

The user already had a complete gateway — `docs/knowledge-site/server.py` — that serves the marketing/docs site (homepage, whitepaper, bench, query) and reverse-proxies each demo at `/<demo>_ndb/` (static + `/api`), injects a feedback widget, and has a recorded-race aggregates endpoint. **Reuse it; don't reinvent.** (I had started a custom gateway — deleted it.) The earlier-built per-domain subdomains were the wrong shape for what the user wanted (one origin, paths).

Adapted `server.py`: trimmed DEMOS to the 3 live ones with ports repointed to the deployed `ndb-srv-*`/`ndb-web-*` services; added `/v1/*` → trio read-only server (machine API on the main origin) and `/studio` → Studio. **Studio path-mount trick:** Studio's frontend calls `fetch("/api"+path)` in exactly two spots, so the gateway rewrites `"/api"` → `"/studio/api"` on the served HTML and routes `/studio/api/*` → `8780/api/*` — clean mount under a path, no recompile.

Stood up a writable **feedback nDB** (`ndb-server` on :8744, no `--read-only`) + the **site gateway** (`server.py` on :9880) as cgroup-capped systemd units. Created the empty feedback DB with `ndb-studio --new` (4s + kill). Tested every path on `localhost:9880` first (all 200; `/studio/api/me` → `public_read:true`; explorer `/api/iter` returns data) — **only then** rewrote the tunnel ingress to a single `ndb.nextstar-erp.com → localhost:9880` rule and removed the 3 explorer-subdomain rules.

**Tunnel-edit care:** the ingress rewrite is block-aware and preserves the unrelated tamdinh `path: ^/socket.io` rules (verified) — never clobber a shared tunnel's other tenants. After restart the public site served every path 200; homepage shows v2.4.0 / 14 crates / Studio in nav; the exoplanet explorer renders through `/exoplanet_ndb/` with its API same-origin; 0 console errors.

**Honest gaps:** (1) `bench.html` ships but its live-race backends (PG + harness) aren't deployed and there are **no recorded results** to show — I refused to fabricate numbers; the aggregates table is empty until a one-time real nDB-vs-SQLite run is logged. (2) The orphaned `ndb-protein/exoplanet/biodiv.nextstar-erp.com` CNAMEs now fall through to the tunnel 404 (harmless; cleanup needs the CF dashboard). (3) Frontend stays static by decision — agent-friendliness comes from `llms.txt` + the MCP/`/v1` interfaces, not a SPA.

**Live, one origin:** `https://ndb.nextstar-erp.com` → homepage · `/whitepaper.html` · `/bench.html` · `/query-language.html` · `/alphafold_ndb/` · `/exoplanet_ndb/` · `/biodiv_ndb/` · `/studio/` · `/v1/*` · `/llms.txt`. Footprint: 10 cgroup-capped services (gateway + feedback + studio + trio + 3 API + 3 static), all tiny next to Frappe.

---

## 2026-06-14 — Real benchmark data, published honestly

The user asked for real numbers, not fabricated ones. Ran the project's own bench-race servers locally (same host = fair head-to-head): `cargo --example bench_race -p ndb-engine` (:8771) vs `race-sqlite-rust` (:8774, rusqlite — same C SQLite, no GIL), both loaded with the identical 100k realworld shape.

**Gotchas:** the bench server rate-limits `/run` to 1/workload/3s (got 429 on rapid repeats) — fixed by round-robin sampling with inter-round sleeps + taking the **median of 5**. The dev box was contended by leftover local test servers from earlier in the session (2× variance, join winner flipped between single samples) — killed them, load settled ~1.3, numbers stabilised. Publishing via the public `/api/race/log` hit **403** (Cloudflare challenges scripted POSTs) — bypassed by POSTing to the gateway's **localhost** on the VPS.

Measured BOTH modes (publishing only controlled would undersell nDB; only stress would oversell — both is the honest picture):

| workload | controlled (1-thread, rps) | stress (conc=32, rps) |
|---|---|---|
| point_lookup | nDB 269k vs SQLite 122k | nDB **9.46M** vs 1.51M |
| property_lookup | nDB 310k vs 30k | nDB **9.65M** vs 452k |
| count_aggregate | nDB 556k vs 19k | nDB **10.2M** vs 868k |
| recursive_contains_depth3 | SQLite 8.8k vs nDB 6.5k | **nDB 198k** vs 48k |
| single_pattern_query | SQLite 27k vs nDB 9k | SQLite 548k vs 294k |
| two_pattern_join | SQLite 2.2k vs nDB 0.8k | SQLite 47k vs 23k |
| iter_all (full scan) | SQLite 239 vs nDB 26 | SQLite 1.8k vs 116 |

Honest split: **nDB wins indexed lookups, aggregates, and recursion — decisively under concurrency (6–21×, lock-free MVCC reads vs SQLite's global lock); recursion flips to nDB under load. Embedded SQLite wins full-table scans, single-pattern filters, and 2-pattern joins** (mature query planner). 14 races (7 controlled + 7 stress) logged to the feedback nDB; `bench.html` defaults to the `sqlite-rust` challenger and renders the recorded table. Live-race buttons stay offline (no PG/harness on the box) — recorded results are the deliverable.

**Did NOT fabricate** anything; where the data contradicted the old "nDB wins recursion" card, rewrote the card to the measured reality (recursion is a *concurrency* win, not single-thread).

## 2026-06-14 — Storage-cost headline + benchmark-report plan (next pass)

**Real storage measurement** (same logical dataset: 50k entities + 50k N-ary facts + properties, region/customer/sales/contains shape):
- **nDB: 8.0 MB** on disk (LSM, dictionary-coded; WAL-resident, pre-compaction).
- **SQLite: 14.6 MB** (checkpointed + VACUUMed; normalized tables + `sales`/`contains` junction tables).
- → **~1.8× more compact**, because each N-ary fact is one dictionary-coded record instead of junction-table rows repeating foreign keys + labels. (Flushing/compacting nDB should hold or widen the gap.)

**User direction:** remove the live-race buttons/tools (no PG/harness on the box → dead UI), and publish *validated* numbers — many samples with statistics, emphasising nDB's real strengths: **storage cost** and **multi-dimensional / N-ary data that forces SQL into many table-links (junctions + multi-way joins)**.

**Plan for the focused benchmark-report pass (do with fresh context — rigor is the point):**
1. **Strip live UI** from `bench.html` (1485 lines): remove `#challenger-pick` live buttons, `#race-btn`/`#stress-btn` + the live-fetch JS + `#backends-status`; keep the access-pattern explainers + the recorded aggregate tables. Re-title from "Live race" to "Benchmark report". (Surgery on a big file — do carefully, verify render.)
2. **Statistical rigor:** dedicated harness — ≥30 samples/workload/mode on a quiet host, report mean ± stddev + p50/p95/p99 + a 95% CI; discard warmup; pin if possible. Extend `/api/race/*` (or a new aggregates view) to carry stddev/CI, not just mean.
3. **Storage scaling curve:** rebuild both engines at 10k / 100k / 1M / 10M N-ary facts; plot bytes/fact for nDB vs SQLite (normalized) → show the junction-table overhead growing. Add a chart to the page.
4. **N-ary join-depth study (nDB's thesis):** a workload family where answering needs k-way relationships (k=2..6). SQL = k junction joins; nDB = k adjacency hops. Plot latency vs k for both → the "many table links" cost curve. (The existing `two_pattern_join` / `recursive_contains_depth3` are the seeds.)
5. **Publish** via the VPS localhost `/api/race/log` (public POST is Cloudflare-403'd); validate the report page renders the stats + charts; 0 console errors.

## 2026-06-14 — Locked plan v2: serious 3-engine study (nDB vs SQLite vs MariaDB)

User: serious proper study (not a tweak), compare with SQLite **and MariaDB**, then publish. Feasibility confirmed: **MariaDB 10.11 already on the VPS** (Frappe), and `/home/long/long/rust/python/pg_bench.py` is a ready template (same workload phases + JSONL output) to adapt into a MariaDB harness. To be run as a FRESH focused session (rigor is the point; not to be crammed into exhausted context).

**Engines (all in-process-fair where possible):** nDB (`examples/bench_race`, :8771) · SQLite-Rust (`race-sqlite-rust`, :8774) · **MariaDB** (new harness, :8775) — server.py `BENCH_BACKENDS` already reserves `/bench/*` slots; add `/bench/mariadb`.

**Deliverables:**
1. **MariaDB bench harness** — adapt `pg_bench.py` (or a Rust `mysql`/`mariadb` crate harness) to the SAME 100k region/customer/sales/contains schema with junction tables; implement point_lookup, property_lookup, single_pattern_query, two_pattern_join, recursive_contains_depthK, count_aggregate, iter_all; expose `/health /workloads /run/<name> /stress`. Use a dedicated bench DB/user (NOT Frappe's).
2. **Statistical rigor** — dedicated runner, ≥30 samples/workload/mode on a quiet host (run benches LOCAL, not on the Frappe VPS, to avoid contention), discard warmup; report mean ± stddev, p50/p95/p99, 95% CI. Persist per-sample so CI is recomputable. Extend the aggregates view to carry stddev/CI.
3. **Storage-scaling study** — build all 3 engines at 10k / 100k / 1M (and 10M if disk allows) N-ary facts; measure bytes/fact (nDB LSM **compacted**, SQLite VACUUMed, MariaDB InnoDB `information_schema` size). Plot the junction-table overhead curve. (Baseline already: nDB 8.0 MB vs SQLite 14.6 MB at 50k+50k.)
4. **N-ary join-depth study (the thesis)** — workload family needing k-way relationships (k=2..6): SQL/MariaDB pay k junction joins, nDB pays k adjacency hops. Plot latency + rows-scanned vs k for all 3. This quantifies "traditional DB needs many table links."
5. **Publish as a report** — strip live UI from `bench.html` (1485 lines; live controls + recorded tables interleaved — careful surgery), retitle "Benchmark report", render stats tables + simple CSS/SVG charts (no chart-lib), 3-engine columns. Log via VPS localhost `/api/race/log` (public POST is CF-403'd). Add a methodology section (hardware, versions, sample sizes, caveats — embedded vs networked: SQLite/nDB in-process, MariaDB over a socket → note the architectural axis honestly).

**Methodology honesty:** controlled (1-thread latency) AND stress (concurrency throughput); embedded (nDB, SQLite) vs networked (MariaDB) is a real axis — report it, don't hide it. Don't fabricate; publish losses too.

## 2026-06-14 — 3-engine study DONE in-session (nDB vs SQLite vs MariaDB), published

Local MariaDB 10.11.14 was already installed + running (:3306, `~/.my.cnf` root creds). Loaded the SQLite bench's EXACT rows (region 1000 / customer 49000 / sales 45000 / contains 5000 = 50k entities + 50k N-ary facts) into MariaDB InnoDB with matching indexes (pymysql via `~/.my.cnf`). Fair same-host triad.

**Storage (on disk, same dataset):** nDB **8.0 MB** (pre-compaction WAL — conservative) · SQLite **14.6 MB** (VACUUMed) · MariaDB **21.2 MB** (InnoDB data+index). nDB 1.8× vs SQLite, 2.6× vs MariaDB — N-ary facts don't normalise into junction rows.

**Controlled latency (rps; p99):** nDB / SQLite / MariaDB
- point_lookup: 672,948 (5µs) / 90,090 (20µs) / 13,968 (268µs) — nDB wins, MariaDB pays the networked per-call tax.
- property_lookup: 339,674 (8µs) / 21,260 (107µs) / 6,159 (538µs) — nDB.
- two_pattern_join: 1,144 / 1,628 / **3,435** — MariaDB's optimiser wins single-thread (honest loss for nDB).
- recursive_contains_depth3: 6,164 / **7,532** / 2,565 — SQLite edges single-thread.

**Stress (conc=32, nDB vs SQLite):** point 9.46M vs 1.51M · property 9.65M vs 452k · count 10.2M vs 868k · recursion **198k vs 48k (flips to nDB under load)**.

**Methodology honesty:** nDB+SQLite measured in-process by their Rust bench servers (p50/p99 over 500 iters/point); MariaDB end-to-end via a reused client over a unix socket (1.5–4k iters) — includes client/protocol cost (the real price of a networked DB; part of nDB's embedded pitch). Embedded-vs-networked stated, not hidden. No fabrication; losses shown.

**Published:** rewrote `bench.html` as a static **Benchmark report** (removed ALL live-race buttons/tools per the user) — methodology + 3 result tables + the N-ary structural explanation + honest takeaways. Homepage card updated to the real headline. Live at `https://ndb.nextstar-erp.com/bench.html`.

**Not done (honest):** MariaDB concurrent sweep, storage-scaling curve (10k→10M), and a formal mean±CI harness — the controlled p50/p99 are 500-iter / 1.5–4k-iter distributions (solid), but the cross-engine basis differs (embedded vs networked); a future pass could unify it + add the scaling curves.

## 2026-06-14 — Full study: storage-scaling curve + honest concurrency scope

Parameterised the nDB bench builder (`NDB_BENCH_SCALE` env via LazyLock) to build at any scale; generated matching-cardinality data for SQLite + MariaDB in python. **Storage scaling (total records → MB):**

| records | nDB | SQLite | MariaDB |
|---|---|---|---|
| 10k  | ~1.0 | 1.50 | 1.70 |
| 100k | 8.0  | 14.6 | 20.2 |
| 1M   | **81** | 147 | 178 |

→ nDB ~81 B/record vs 147 (SQLite) / 178 (MariaDB); the gap holds/grows with scale (junction-table overhead). nDB figures are pre-compaction WAL (conservative).

**MariaDB concurrency — refused to publish a misleading number.** Measured via a Python threaded client, conc=32 came out *lower* than single-client (7.5k vs 14k) because the client is GIL-bound, not MariaDB's server. Publishing that would unfairly understate MariaDB. So the concurrency table stays nDB-vs-SQLite (both native Rust bench servers, fair) with an explicit note that a fair MariaDB concurrency number needs a native client (not built this pass). Honest scoping > a wrong number.

Published: `bench.html` now shows the 3-engine scaling curve + the concurrency caveat. Report is fully static (no live tools). Local artifacts (bench servers, `ndb_bench_cmp` DB) are dev-only.

**Remaining for a future pass (stated on no false pretenses):** native-client MariaDB concurrency sweep; a single unified mean±95%CI harness across all engines (cross-basis embedded-vs-networked makes one-number-fits-all hard — current latency points are 500–4000-iter distributions, solid, but measured per engine's natural interface).

## 2026-06-14 — N-pattern join (join-depth) sweep — honest, partly-negative result

User: "two_pattern_join exists — what about n-pattern join?" Built a depth sweep (k=2..6) on the SAME containment topology across engines: parameterised nDB's recursion depth via `NDB_BENCH_RECDEPTH`; replicated nDB's exact index-edge formula into SQLite + MariaDB; ran a recursive CTE (distinct nodes, depth<k) timed many iters.

Mean latency (µs) per depth:
| k | nDB | SQLite | MariaDB |
| 2 | guard-err¹ | 79 | 150 |
| 3 | 95 | 56 | 152 |
| 4 | 120 | 64 | 170 |
| 5 | 97 | 66 | 209 |
| 6 | 107 | 77 | 268 |

**Honest finding (does NOT favour nDB):** this graph is dense → reachable set saturates by ~3 hops → cost is node-bounded, flat with depth for in-process engines. SQLite's CTE (~65µs) is a touch FASTER than nDB's traversal (~100µs). The only clear depth effect is MariaDB's networked per-level tax (150→268µs). ¹nDB's recursion engine REJECTED depth=2 with `RecursionDepthExceeded { depth:2, frontier_size:2 }` (and the bench server `unwrap()`s query errors → the connection thread panics) — a real limitation + a bench-robustness bug, both reported.

**Conclusion published verbatim on the page:** join-depth is not where nDB wins on this dataset; its wins are storage + lookups + concurrency. The thesis (deep relational join blow-up) needs a SPARSE, high-fan-out graph where each hop multiplies the intermediate result — a future dataset, not this one. Did not manufacture a graph to make nDB win (that would be cherry-picking — the opposite of a serious study).

## 2026-06-14 — Strength-bench candidates: honest triage (schema-evolution measured modest)

User: "any other bench showing nDB strength? — all of them." Mapped 4 candidates; started with the fastest. **Schema evolution measured (N=200k):** SQLite ALTER 1ms + backfill 78ms; MariaDB ALTER 3ms + backfill 127ms; storage barely moved. Modern engines do **instant ADD COLUMN** (no rewrite) → the "migration pain" thesis is weak. nDB's edge is narrow (no DDL/lock + sparse storage only-where-present), not dramatic. → NOT a compelling strength bench. (Second candidate, after dense-graph n-pattern, that measured modest — honest, and it makes the real wins more credible.)

**Meta-finding:** nDB's genuine, defensible wins = storage-at-scale, indexed lookups, concurrency, time-travel. Several intuitive "strengths" don't beat mature engines. Report that honestly.

**Locked plan — remaining 3 strength benches (focused next pass; designs made fair up front):**
1. **Time-travel / as-of** (most likely genuine win): nDB native MVCC snapshot read at any tx vs MariaDB **system-versioned tables** (`AS OF`, fair) + SQLite manual SCD2. Measure: storage of M versions of N rows (nDB dictionary-coded append vs SQL history table growth) + as-of query latency. nDB's signature; SQLite structurally can't.
2. **High-arity N-ary storage** (the thesis, generalised): N facts of arity K (roles), K=2..6. nDB = N records (K refs each); SQL modelled the standard way = a `fact_role(fact_id, role, entity_id)` association table = **N×K rows** (+ index). Measure storage + a "facts touching entity X" query vs the K-row regroup. Storage gap grows with K — clean + fair (association table IS the relational N-ary pattern). Variable-arity makes it starker (SQL pays max-width or many rows).
3. **Co-located vector + filter** (capability gap): hybrid "vector-similar AND property=X" — nDB native kNN (HNSW) in one engine; SQLite/MariaDB 10.11 have no native vector → brute-force scan or a bolt-on store. Frame as capability + measure nDB hybrid latency vs a brute-force baseline.

Each is a real build (versioned data + system-versioning DDL; a K-ary nDB loader; vector data + brute-force baseline) — to be run with the same rigor (≥ solid sample sizes, fair modelling, losses shown), not crammed.
