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
