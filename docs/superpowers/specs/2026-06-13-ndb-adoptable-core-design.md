# nDB Adoptable Core — Design

**Date:** 2026-06-13
**Status:** Approved (brainstorming), ready for implementation planning
**Sequence position:** Sub-project 1 of the "general-purpose DBMS product" roadmap (Stages 1–3 of the adoption ladder). Later sub-projects: **Deploy & Operate** (Stage 4) and **Scale** (Stage 5).

## Goal

An external developer or AI coding agent can **install nDB, use it from JavaScript/TypeScript, point an agent at it, and trust their data survives upgrades** — the gate that turns the existing engine (13 crates, 698 tests, v1.3.0) into an adoptable product.

This spec closes specific gaps on top of what already ships. It does **not** rebuild existing capability.

### What already exists (do not rebuild)
- Engine: WAL+fsync durability, crash recovery, MVCC+SSI, auto-compaction, AES-GCM at-rest encryption.
- Server (`ndb-server`): 16 JSON-over-HTTP routes, conn caps, timeouts, TLS, ReBAC, audit log, graceful shutdown, `/health` `/ready` `/metrics` `/status`.
- Clients: Rust (`ndb-client-rust`, ~18 typed methods) + Python; `ndb-cli`.
- Agent: `ndb-mcp-server` — MCP/JSON-RPC 2.0 over stdio, hyperedge tools, time-travel, pagination, JSON tool schemas, resources & prompts, ReBAC + audit.
- Format versioning machinery: per-record `FORMAT_VERSION = 3` byte, decoder dispatch, `FORMAT_VERSION_MAX_SUPPORTED`, rejects unknown-future versions; v2 and v3 decoders coexist.
- Arrow export (`ndb-arrow`) for the GPU/data stack; aarch64 CI guard.

## Success criteria (acceptance tests)

1. From a clean machine: `docker run …/ndb` → `GET /v1/health` returns `200` in **under 60 seconds**.
2. `npm i @ndb/client` → a ~10-line TS program performs `commit` + `query` against the server, working in **Node and a browser/Deno** runtime.
3. `npx @ndb/mcp --path ./db` → an MCP client (Claude/Cursor/Codex) connects, lists tools, and runs a hyperedge commit + a time-travel read.
4. A checked-in **older-format (v2-era) fixture database opens and reads correctly** on the current build — proving the "your data always opens" promise.
5. Every `/v1/` route is documented in `docs/PROTOCOL.md` with request/response schemas; the bare (unprefixed) routes still work as **deprecated aliases**.
6. `docs/COMPATIBILITY.md` states the semver policy across all four versioned surfaces (server, protocol, on-disk format, SDK).

## Components

Each component is independently testable with a clear interface.

### 1. Wire protocol v1
- Add a `/v1/` route layer in `ndb-server` as a **thin shim** over the existing handlers (no handler logic duplicated).
- Keep the existing 16 bare routes (`/commit`, `/read/`, `/iter`, `/query`, `/query/text`, `/query/explain`, `/lookup`, `/traverse`, `/flush`, `/compact`, `/health`, `/ready`, `/metrics`, `/status`, `/replicate`, `/subscribe`, plus `/arrow/*`, `/admin/shutdown`) working as **deprecated aliases** that delegate to the same handlers.
- Write `docs/PROTOCOL.md`: for every `/v1/` route — method, path, JSON request shape, JSON response shape, error envelope, auth/capability required.
- **Versioning rule:** within protocol major `v1`, only additive/backward-compatible changes; a breaking change ships as `/v2/` served *alongside* `/v1/` so live clients never break.
- **Test:** a conformance test that exercises every `/v1/` route end-to-end against a spawned server and asserts the documented response shape.

### 2. Format compatibility guarantee
- Write `docs/COMPATIBILITY.md` committing the **"your data always opens"** promise: every future release retains readers for all prior on-disk format versions; old databases open with no migration step. Additive format changes never break readers; a true breaking on-disk change is a rare engine **major** bump.
- Define a **semver policy** spanning four surfaces, each versioned independently and stated explicitly:
  - **Engine/server** (the `nDB` release version, currently 1.3.0)
  - **Wire protocol** (`v1`, in the URL)
  - **On-disk format** (`FORMAT_VERSION`, currently 3)
  - **SDK** (`@ndb/client` npm version)
- Formalize the existing `FORMAT_VERSION` / `FORMAT_VERSION_MAX_SUPPORTED` constants into the documented contract (link code ↔ policy).
- **Test:** commit a small **v2-era fixture database** under `crates/ndb-engine/tests/fixtures/` and a test that opens it on the current build and reads expected records. This test is the executable form of the promise and must never be deleted.

### 3. `@ndb/client` — TypeScript SDK
- New package at `clients/ts/` (workspace-independent; its own `package.json`, `tsconfig`, build).
- **Thin typed HTTP client** mirroring the Rust client's surface over `fetch`, targeting `/v1/`:
  `health`, `commit`, `read(uuid)`, `iter`, `lookupByKey`, `vectorSearch`, `propertyLookup`, `propertyRange`, `traverse`, `query`, `queryText`, `flush`, `compact`, plus `withToken` / timeout / retry config (GET retries fully; writes retry connection-only, mirroring the Rust client's never-double-apply semantics).
- **Zero runtime dependencies**; ESM; works in Node ≥18, browsers, Deno, and edge runtimes (anything with `fetch`).
- Typed request/response interfaces that match `docs/PROTOCOL.md` exactly.
- **Test:** an integration suite that spawns a real `ndb-server` (via the release binary or `cargo run`) in CI and exercises the full surface against it; plus type-level checks.
- **npm name:** `@ndb/client`. **Fallback** if the `@ndb` scope cannot be claimed: unscoped `ndb-client`.

### 4. `@ndb/mcp` — agent packaging
- npm package wrapping the existing `ndb-mcp-server` binary so `npx @ndb/mcp --path ./db` launches it with no Rust toolchain on the user's machine.
- The wrapper resolves the correct prebuilt binary for the host platform via **per-platform `optionalDependencies`** (the esbuild/swc pattern: `@ndb/mcp-linux-x64`, `@ndb/mcp-linux-arm64`, `@ndb/mcp-darwin-arm64`, each shipping one binary; npm installs only the matching one), then `exec`s it, passing through args/stdio. Chosen over a `postinstall` download-from-release because it works offline, behind corporate proxies, and with `npm ci` lockfile integrity.
- **npm name:** `@ndb/mcp`. **Fallback:** unscoped `ndb-mcp`.
- **Test:** a smoke test that runs the published-shape package against a temp DB and asserts an MCP `tools/list` + one `ndb.commit_hyperedge` + `ndb.read_as_of` round-trip succeeds.

### 5. Packaging & distribution
- **Docker image:** multi-arch (`linux/amd64` + `linux/arm64`) for `ndb-server`, pushed to `ghcr.io/goldrag1/ndb` on git tag. `ENTRYPOINT` runs the server; `--path` mounts a volume. Documented `docker run` one-liner.
- **Release binaries:** a `.github/workflows/release.yml` triggered on `v*` tags, building and attaching to the GitHub Release: `ndb-server` + `ndb-cli` + `ndb-mcp-server` for `linux-x86_64`, `linux-aarch64`, `macos-aarch64`.
- **npm publish:** `@ndb/client` and `@ndb/mcp` published from the same tag workflow.
- **Smoke test in CI:** after building the image, `docker run` it and `curl` `/v1/health` expecting `200`.
- **macOS:** arm64 only (Apple Silicon). Intel-Mac binaries omitted; add later only if requested.

### 6. Getting-started documentation
- Rewrite `docs/QUICKSTART.md` into two ≤5-minute paths:
  - **Human developer:** `docker run` the server → `npm i @ndb/client` → a hello-world TS snippet (commit + query) → pointer to a small example app under `examples/ts-app/`.
  - **AI agent:** `npx @ndb/mcp --path ./db` → connect Claude/Cursor/Codex (config snippet) → one hyperedge round-trip showing the distinctive model.
- Cross-link `docs/PROTOCOL.md` and `docs/COMPATIBILITY.md` so a developer can find the contract.

## Explicit non-goals (deferred to later specs)

- ORM / query-builder / high-level idiomatic layer over the thin client.
- Go and other-language SDKs.
- Helm charts / k8s manifests / docker-compose / systemd units.
- Automatic failover / leader election (Raft).
- Horizontal sharding.
- 10GB+ scale performance work (the explorer-path latency gap).
- GPUDirect Storage.

These belong to the **Deploy & Operate** (Stage 4) and **Scale** (Stage 5) sub-projects that follow this one.

## Architecture notes

- The `/v1/` shim and aliases live in `ndb-server`; no engine change. The protocol is a stable surface over an internally-free-to-evolve engine.
- The TS SDK is a separate build artifact, not a Cargo workspace member; it depends only on the documented protocol, never on Rust internals — so it can be versioned and released independently.
- The format-compat test is the single source of truth for the "always opens" promise: it is an executable contract, not prose.
- Packaging is CI-driven and tag-triggered; no manual release steps, so the "60-second install" path can't silently rot.

## Testing & verification summary

| Component | Verification |
|---|---|
| Wire protocol v1 | Conformance test over every `/v1/` route; alias delegation test |
| Format guarantee | v2-era fixture DB opens + reads on current build |
| `@ndb/client` | Integration suite vs a spawned `ndb-server`; type checks |
| `@ndb/mcp` | `tools/list` + hyperedge commit + as-of read smoke test |
| Packaging | CI builds image + binaries on tag; `docker run` + `/v1/health` smoke test |
| Getting-started | Both quickstart paths executed manually before release; snippets copy-paste runnable |

## Open items resolved (placeholders, decided)

- npm names: `@ndb/client`, `@ndb/mcp` (fallback unscoped if scope unavailable).
- Docker registry: `ghcr.io/goldrag1/ndb`.
- macOS: arm64 only.
