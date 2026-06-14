## Session 2026-06-14 (b) — Closed all 7 productization gaps

Branch `feat/ndb-production-deploy` (origin goldrag1/nDB). Pushed: `e82235d..43266c4`.
8 gap commits + 1 gitignore cleanup. Staged only my files throughout (the
concurrent session's `.pending-promotions.md` / `.multi-session/*` untouched).

### Gaps closed (all verified — tests + live)
- [x] **Gap 1** `9cd4795` — MCP conformance suite (`crates/ndb-mcp-server/tests/mcp_conformance.rs`).
  Spec-client asserts initialize/tools-list-schema/**tools-call content envelope**/resources/prompts/JSON-RPC error codes. Proven to FAIL pre-fix; CI gate added.
- [x] **Gap 2** `34b86d9` — unified `/v1`+`/mcp` one engine. `Server::from_shared` / `McpServer::from_shared` + `ndb-mcp-server --v1-bind <addr>`. Acceptance test `unified_engine.rs`: /mcp write → /v1 read live.
- [x] **Gap 3** `cb9d82e` — bounded thread-per-connection for MCP HTTP/HTTPS (scoped threads, `NDB_MCP_MAX_CONN` cap, default 256). Single-writer preserved by RwLock. Tests: concurrent-commits + cap.
- [x] **Gap 4** `a3891e6` — first-class config: `ndb.register_vector_property` (persists to `<db>/.vector_props`, reloaded+rebuilt on open) + `ndb.define_schema` (names the catalog). Drops `NDB_VECTOR_PROP` (kept as deprecated migration aid). systemd `${VAR}` gotcha documented. Tests: `config_tools.rs`.
- [x] **Gap 5** `6f5e788` + `0bb81ae` — Python SDK `query_text` + server-side `arrow_export()`→pyarrow→pandas/polars; `docs/TUTORIAL-first-app.md`; `install.sh --local` (one-command zero→first-query) + prod `DOMAIN EMAIL` chaining. Edge made 3-way (`/mcp` | `/v1`+`/arrow`+`/health` | UI).
- [x] **Gap 6** `135c18a` — multi-tenant auth: `NDB_MCP_PRINCIPALS` registry (token→principal+caps), per-request resolution, capability gating, audited under dev name. `handle_line_as(line, effective)`. Test: `two_devs_different_capabilities_audited`. Opt-in; NOT on the live box (its agents write tokenless).
- [x] **Gap 7** `06873ee` — pin pingora `=0.8.1` / fastembed `=4.9.1`; CI `--locked`; new jobs: `standalone-crates` (builds ndb-edge+embed) and `deployed-stack-smoke` (unified bin, /v1+/mcp+read-back). fmt extracted to its own ADVISORY job (see backlog below).

### Live deploys done this session (reversible, auto-rollback in the deploy scripts)
- **Unified `ndb-mcp-server`** on longsv1: ExecStart now `--http 127.0.0.1:9000 --v1-bind 127.0.0.1:8742` — ONE process serves /mcp + /v1 from one engine. (`.bak-<ts>` of binary + unit kept.)
- **3-way Pingora edge** redeployed: `/etc/ndb/ndb.env` now sets `NDB_DATA_UPSTREAM=127.0.0.1:8080` (UI), `NDB_API_UPSTREAM=127.0.0.1:8742` (data API), `NDB_MCP_UPSTREAM=127.0.0.1:9000`. (`.bak-<ts>` of binary + env kept.)
- Verified through the public edge: `/` 200, `/mcp` 405, `/v1/health` 200; `pip install 'ndb-client[arrow]'` + 5-line example pulls ~1.4k live records into pandas.

### Trạng thái hiện tại
All 7 gaps closed, committed, pushed. 88 tests pass `--locked` across ndb-server +
ndb-mcp-server. Live box healthy post-deploy (UI/`/mcp`/`/v1` all green). Two writers
still grow memory (ndb-agent sim 15s on longsv1; real Claude agent 30 min on control laptop).

### Next Session Task (candidates, pick by priority)
1. **Clear the rustfmt backlog** (~25 files predating current stable rustfmt) with
   `cargo fmt --all`, then flip the CI `fmt` job from advisory → blocking (drop
   `continue-on-error`). Same for the clippy backlog. This is what currently keeps
   the format/lint gates non-blocking. NOTE: coordinate with the concurrent session
   (the backlog spans files both sessions touch).
2. Optionally tighten the live box: it's open-write (tailnet-gated). Gap 6 gives the
   mechanism — a `NDB_MCP_PRINCIPALS` registry — but enabling it would lock out the
   tokenless sim/remember agents, so it needs the agents to carry a token first.
3. Resharding / Raft / follower-mode hardening (longer-horizon, per productionization notes).

### Access / conventions (critical)
- longsv1: `ssh oldserver` (key-only), sudo pw `12`. Build on control laptop → scp binaries → staged sudo (NEVER pipe pw + heredoc together). Heavy crates standalone under deploy/sovereign/{ndb-edge,embed}.
- Live config /etc/ndb/ndb.env; services ndb-edge/ndb-mcp/ndb-static/ndb-embed/ndb-agent. Frappe disabled (data preserved).
- A concurrent session may share the branch — stage only your files; never `git add -A`.
- Pre-existing fmt backlog: keep your own files fmt-clean (`rustfmt --edition 2024 <file>`); don't mass-reformat others' files on the shared branch.

---

## Session 2026-06-14 (a) — Pingora + nDB sovereign stack, MCP, live agent, vectors, viz

### Đã làm
- Researched "Rust webserver + AI-agent-friendly" → mapped to nDB; chose agent-memory vertical (office-hours).
- Built network MCP transport (Streamable HTTP) + native rustls TLS in ndb-mcp-server.
- Sovereign deploy package `deploy/sovereign/` (Pingora edge crate, systemd units, certs, README).
- Installed full Rust stack on **longsv1** (cleared Frappe, reversible); live at https://ndb-pingora.nextstar-erp.com/ (real LE cert via DNS-01 over a DNS-only A→tailnet IP; no tunnel).
- Agent-memory demo: Rust seed/static/agent, 3D viz (hyperedges + time-travel + attribute dims + vector recall), live + scheduled real Claude agent (systemd user timer every 30 min, on the control laptop, via the ndblive MCP).
- Real embeddings: `deploy/sovereign/embed` (fastembed BGE-small 384-dim) + ndb-mcp auto-embeds text on write + `ndb.search_text`. recall@5 = 1.0; genuine semantic recall verified.
- Fixed MCP spec gap surfaced by a real Claude agent: tools/call now returns the `content` envelope (dual-shape).

### Trạng thái (đã được session (b) tiếp nối + đóng hết gaps)
Working prototype, fully live on longsv1, committed+pushed to `feat/ndb-production-deploy`.
PR #2 merged to main earlier; this branch has since advanced. The 7 productization gaps
listed here (Gaps 1–7, originally in `docs/CLOSE-GAPS-PROMPT.md`) are now ALL closed — see session (b) above.
