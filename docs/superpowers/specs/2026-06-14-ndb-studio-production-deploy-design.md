# nDB in Production on a Tiny VPS — Design

**Date:** 2026-06-14
**Status:** approved design, pre-plan
**Goal:** Put nDB into real public production on the resource-constrained HTS VPS, reachable at `ndb.nextstar-erp.com`, with **both** a human web GUI (nDB Studio, anonymous read) **and** the machine `/v1` API (ndb-server, read-only) so that an outside developer or AI coding agent can adopt and test it with the exact published tooling (`npx`, the TS SDK, the `ndb` CLI, the MCP server, `curl`). The whole effort is documented as it happens to produce a how-to **article**.

## One-sentence summary

On one hostname, path-route `ndb.nextstar-erp.com/v1/*` to a read-only `ndb-server` (the `/v1` wire API every published client targets) and everything else to an anonymous-read `ndb-studio` GUI — both cgroup-capped systemd services, each opening its own copy of one merged read-only nDB (AlphaFold proteins + exoplanets + biodiversity) — exposed through the existing `cloudflared-tamdinh` tunnel, then validated clean-room as an external adopter and written up.

## Context & hard facts (measured)

- **Host:** HTS VPS `72.61.119.96`, Ubuntu 24.04.3, 2 cores, 7.8 GB RAM (~4.8 GB free; Frappe ~2.9 GB), 43 GB free disk. SSH `ssh -i ~/.ssh/vps_das frappeuser@72.61.119.96`.
- **Good-neighbor rule:** nDB must never crowd out Frappe. Enforced by cgroup hard caps, not by trusting headroom.
- **Ingress:** `cloudflared-tamdinh.service` already runs (tunnel `447eb352-d6f6-4aaa-a259-1806f8c0992e`, `protocol: http2`) and already serves `tamdinh-vps.nextstar-erp.com` — so `nextstar-erp.com` is in this tunnel's Cloudflare account. Reuse it; no new tunnel, no nginx/certbot (Cloudflare terminates TLS). Its config already path-routes (`path: ^/socket.io`), so per-path routing on one hostname is a proven pattern here.
- **Datasets:** prebuilt nDB DBs in `.demo-data/`: `alphafold-ndb` (332K), `exoplanet-ndb` (80K), `biodiv-ndb` (124K). The 29 GB `langgraph-oa-ndb` is **excluded** (can't be snappy here; prior notes confirm 10 GB is already unusable live).

## Protocol reality (why two binaries)

- `ndb-server` speaks **`/v1/<route>`** (wire-protocol v1; bare `/<route>` are deprecated aliases). This is what the TS SDK (`baseUrl + "/v1"`), the Rust client, the `ndb` CLI (`--url`/`NDB_URL`), and `curl` target.
- `ndb-mcp-server` opens the engine **in-process** (`Engine::open_from_env`) — **no network mode**. An agent runs it (`npx @n-dimension-database-ndb/mcp`) against a **local DB directory**, not a URL.
- `ndb-studio` speaks its own **`/api`** cookie surface + serves the `/` web UI. The SDK/CLI/MCP do not talk to it.
- The engine is **single-writer**: Studio and ndb-server cannot open the same on-disk DB at once. Since both serve **read-only** public content, each opens its **own copy** of the merged DB — no writes in normal operation, so copies never diverge. The operator updates by regenerating the merged DB and redeploying.

Path surfaces don't collide: Studio = `/` + `/api/*`; server = `/v1/*`. So one hostname path-routes `^/v1` → server, default → Studio.

## Components

### 1. Merge tool + UUID-preserving store writes (code)

Entity ids are random UUIDs (global; no cross-DB collision), but type/property/role **names** intern to per-DB integer ids — a raw merge collides those namespaces. So the merge round-trips through **names** to unify dictionaries while **preserving entity UUIDs** so `$ref` values and hyperedge fillers stay valid.

- `Store::create_with_id(id: Uuid, kind, props, author)` — variant of `create()` using the supplied `EntityId` instead of `EntityId::now_v7()`; same name-interning (`Allocator`) and value path.
- `Store::create_hyperedge_with_id(id, kind, entity_roles, edge_roles, props, author)` — same for hyperedges.
- A small merge step (a `tools/merge-demo` binary, or a hidden `ndb-studio --merge <out> <src>...` subcommand): for each source DB, read all records at the current snapshot in **name-keyed** form (kind/property names + values incl. `{"$ref":uuid}` / `{"$vec":[…]}`) via the existing catalog/record projection, then write into the target via `create_with_id` — plain entities first, then hyperedges, then edges-on-edges last, so fillers exist before referencing edges.
- Output: one `studio` DB. Built once, locally. Reproducible from the merge tool + the three source DBs.

### 2. Anonymous read in Studio (code)

- New `--public-read` flag → `public_read: bool` in HTTP `State`.
- `guard_read(session, f)`: when `session.is_none()`, allow the read **iff** `public_read` (else keep 401). `writer()`/`admin()` guards unchanged — writes and user-admin still need a logged-in editor/admin.
- Frontend (`web/index.html`): when `/api/me` is `{authed:false}` and the server is public-read, render the data UI **read-only** (no login wall); hide create/edit/delete/bulk/admin affordances; show a "Log in" entry for editors. (Confirm in planning whether the current UI hard-blocks on no session.)
- Read-only query console stays available to anonymous users; runaway reads bounded by the cgroup CPU cap. Reserved `$User`/`$author` already filtered from all data views.

### 3. Standalone read-only mode in ndb-server (code)

- Today `with_read_only(true)` is only set under `--replicate-from` (follower). Add a standalone **`--read-only`** flag that calls `server.with_read_only(true)` without requiring a leader.
- Run the public server `--read-only` with **no token** → open anonymous `/v1` reads, writes return `403 read_only`. Symmetric with Studio's `--public-read`. (Confirmed: `auth_token=None` + no principals registry ⇒ no auth gate, so reads are open; read-only blocks writes.)

### 4. Runtime + resource safety (ops) — two capped services

Layout under `/home/frappeuser/ndb/`: `bin/{ndb-studio,ndb-server}`, `studio-db/` and `server-db/` (each a copy of the merged DB), `backups/`.

- `ndb-studio.service`: `ndb-studio --low-memory --no-open --public-read --bind 127.0.0.1:8780 /home/frappeuser/ndb/studio-db`
- `ndb-server.service`: `ndb-server --read-only --bind 127.0.0.1:8742 --path /home/frappeuser/ndb/server-db`
- Both: `User=frappeuser`, `Restart=on-failure`, cgroup caps `MemoryHigh=256M`, `MemoryMax=384M`, `MemorySwapMax=0`, `CPUQuota=50%` **each** (≈768 MB / 1.0 core worst case combined on a 2-core box — Frappe keeps ≥1 core under contention; the DB is <600 KB so real RSS/CPU sit far below). Either service OOMs itself before starving Frappe.
- Studio bootstrap: capture the one-time admin password from `journalctl -u ndb-studio`, create the operator admin (anonymous covers all viewing).

### 5. Ingress (ops) — one hostname, path-routed

- `cloudflared tunnel route dns 447eb352-d6f6-4aaa-a259-1806f8c0992e ndb.nextstar-erp.com` (CNAME, no dashboard).
- Add to `~/.cloudflared/config-tamdinh.yml` **above** the `http_status:404` catch-all:
  ```yaml
  - hostname: ndb.nextstar-erp.com
    path: ^/v1
    service: http://localhost:8742
  - hostname: ndb.nextstar-erp.com
    service: http://localhost:8780
  ```
- `sudo systemctl restart cloudflared-tamdinh.service`. Host header passes through; same-origin so no CORS. (`/v1/health` → server strips `/v1` → `/health`.)

### 6. Build & ship (ops)

- Build `ndb-studio` and `ndb-server` `--release` locally targeting **`x86_64-unknown-linux-musl`** (static, glibc-independent). Keeps the Rust build's CPU/RAM spike off the Frappe box.
- `scp` both static binaries + two copies of the merged DB. Install the two systemd units. Start, capture admin password, verify.

### 7. Adopter artifacts (so the clean-room test + article are real)

- Publish the merged DB as a downloadable tarball (e.g. a GitHub release asset `ndb-demo-trio.tar.gz`) so an external agent can run the **local** MCP against it. (The repo's `.demo-data/` + merge tool is the from-scratch path.)
- Confirm the published npm packages and the `ndb` prebuilt binary are the current release the article will tell readers to install.

### 8. Backups (ops, light)

- Nightly `tar` of `studio-db/` + `server-db/` to `backups/` with 7-day rotation. Belt-and-suspenders — the DBs are reproducible from the merge tool.

## Data flow

```
3 source DBs (.demo-data/{alphafold,exoplanet,biodiv}-ndb)
        │  merge tool (read by name · write create_with_id · preserve UUIDs)
        ▼
   merged DB ──► two copies ──scp──► VPS /home/frappeuser/ndb/{studio-db,server-db}
                                          │                    │
              ndb-studio --public-read 8780                 ndb-server --read-only 8742
                                          │                    │
                         cloudflared-tamdinh (http2), one hostname, TLS
                                          ▼
                       ndb.nextstar-erp.com  ──┬── /v1/*  → ndb-server  (machines)
                                               └── else   → Studio GUI  (humans)
              humans: anonymous read · machines: open /v1 reads · all writes: 403
```

## The adoption / validation gate (the point of the second binary)

Run from a **scratch directory, with nothing pre-installed**, as if a brand-new developer or AI coding agent adopting nDB for the first time — every command and its output captured into the article journal:

1. **TS SDK** — `npm i @n-dimension-database-ndb/client`, then a tiny script: `new NdbClient("https://ndb.nextstar-erp.com")` → `health()`, a read query, and a write attempt that must surface `403 read_only`.
2. **`ndb` CLI** — download the prebuilt binary, `ndb --url https://ndb.nextstar-erp.com health` + a query; confirm it pretty-prints and that a commit is rejected read-only.
3. **Raw `curl`** — `curl https://ndb.nextstar-erp.com/v1/health`, a query POST, and a commit POST returning 403. (No-tooling baseline anyone can reproduce.)
4. **MCP** — `npx @n-dimension-database-ndb/mcp` against a downloaded copy of `ndb-demo-trio.tar.gz`; exercise the agent tools (entity lookup, neighbors, hyperedge query) and confirm an AI agent can reason over the trio.
5. **Human GUI** — open `https://ndb.nextstar-erp.com/` logged-out; confirm read-only exploration of all three domains (table, 360° view, graph, time-travel) and that edit affordances are absent until login.

Gate passes only when all five legs work from clean room **and** writes are uniformly refused on the public surfaces.

## Lessons capture → how-to article (deliverable)

This deployment is the source material for **"How an AI agent + developer put a Rust database into production on a tiny shared VPS — and made it adoptable."** Treat it as a documented expedition.

- Running journal at `docs/articles/2026-06-nDB-on-a-tiny-vps-journal.md`, appended **as each step happens** (verbatim errors, exact `free -h` before/after, the musl build invocation, the `cloudflared route dns` + ingress edit, the bootstrap-password capture, each clean-room command + output). Reconstructed-after-the-fact ops writing loses the timestamps and real error text that are the whole value.
- For every gotcha: symptom (verbatim) → wrong assumption → actual cause → fix → generalizable rule.
- Decisions already worth narrating: reuse-existing-tunnel vs new tunnel; one-hostname path-routing for "point anything at one URL"; merge-into-one-DB (forced by Studio's process-global, admin-only switching) vs three-instances vs per-request `?db=`; the symbol-dictionary-vs-UUID merge subtlety; **two binaries because GUI(`/api`) ≠ wire API(`/v1`) and the engine is single-writer**; anonymous read as small symmetric flags (`--public-read` / `--read-only`); cgroup capping as the good-neighbor guarantee.
- Synthesize into `docs/articles/2026-06-nDB-on-a-tiny-vps.md`: prerequisites, step-by-step with real commands, the traps and how to detect them, the clean-room adopter walkthrough, and a "what breaks this at 10×" honesty section.
- Promote cross-project-general lessons into `~/.claude/rules/*.md` at session close (mistake-capture protocol), so the agent gets smarter too.

## Error handling & edge cases

- **Refs/edges after merge:** preserved via kept UUIDs + ordering (entities → hyperedges → edges-on-edges). `/api/integrity` (Studio) and a `/v1` integrity read on the merged DB are the acceptance gate (zero dangling refs / bad fillers).
- **Public writes:** impossible — Studio gates writes behind login; ndb-server runs `--read-only` (403). Both verified in the gate.
- **Heavy anonymous reads:** bounded by `CPUQuota`.
- **Restart logs Studio users out** (in-memory sessions) — acceptable.
- **cgroup OOM:** service restarts rather than swapping into Frappe's space (won't trigger — DB <600 KB).
- **Tunnel coupling:** nDB's two ingress rules live in tamdinh's config; documented, minimal, reversible (`nextstar-erp.com` is a temporary domain per the requirement).

## Testing / acceptance

1. **Local:** merged DB opens; catalog shows all three kinds; integrity clean; Studio anonymous (`--public-read`, no cookie) reads each kind + runs a query, and `/api/commit` without a session is refused; ndb-server `--read-only` answers `/v1/health` + a query, and `/v1/commit` returns 403.
2. **VPS:** both services active under cgroup caps; `free -h` shows Frappe headroom unchanged; `https://ndb.nextstar-erp.com/` loads the read-only GUI logged-out and all three domains explore; `https://ndb.nextstar-erp.com/v1/health` answers; login unlocks edit; query burst stays under the caps.
3. **Clean-room adoption gate:** all five legs above pass from a scratch dir.
4. **Good-neighbor check:** concurrent reads, confirm both services stay under cap and Frappe (`erp.htsfood.com` / tamdinh) is unaffected.

## Out of scope (explicitly)

- The 29 GB OpenAlex tier (later, beefier host).
- Per-session/per-request multi-DB selection (moot once merged).
- Live read-write replication between the two copies (read-only demo doesn't need it).
- nginx/certbot (Cloudflare handles TLS); a Tauri shell (the frontend↔backend seam allows it later); MCP-over-HTTP (MCP is local-only today).

## Implementation order (for the plan)

1. `Store::create_with_id` / `create_hyperedge_with_id` + tests.
2. Merge tool; produce + integrity-check the merged DB locally.
3. Studio `--public-read` + `guard_read` change + frontend read-only-for-anon; test anonymous read / gated write locally.
4. ndb-server `--read-only` flag; test open reads + 403 writes locally.
5. musl release builds of both binaries.
6. VPS: scp binaries + two DB copies, two systemd units with caps, start, bootstrap admin.
7. DNS route + path-routed tunnel ingress + restart; verify GUI + `/v1` publicly.
8. Publish `ndb-demo-trio.tar.gz`; run the clean-room adoption gate; capture everything.
9. Synthesize the article; promote general lessons to rules at session close.
