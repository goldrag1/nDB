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
