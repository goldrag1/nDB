# nDB Compatibility & Versioning Policy

The promise that makes nDB safe to build on: **your data always opens.**
This document states the guarantee and the semver policy across every
versioned surface. Each guarantee is backed by a test — the contract is
executable, not just prose.

## The core promise: your data always opens

**Every future release of nDB retains readers for all prior on-disk format
versions. A database written by any past version opens on any later version
with no migration step.**

- Additive on-disk format changes never break older readers, and never
  require a reader upgrade to *keep* reading old data.
- A new format version is only introduced for genuinely new capability, and
  the new-version decoder is written so that records from every prior version
  still decode (older fields default to empty/absent).
- A change that could *not* be made backward-compatible would be a rare
  **engine major** bump, shipped with a migration tool — but the design goal
  is to never need one.

### How it's enforced in code

- Every record carries a 1-byte `format_version` in its envelope
  (`crates/ndb-engine/src/record.rs`). Decoders dispatch on it.
- `FORMAT_VERSION` is the version this build *writes* (currently `3`).
- `FORMAT_VERSION_MAX_SUPPORTED` is the highest version this build can *read*
  (currently `3`). A record with a higher version is rejected cleanly
  (`UnsupportedVersion`) rather than mis-decoded — a forward guard, so an old
  binary fails loud instead of corrupting.
- The decoder reads the new-version trailer only when
  `format_version >= N`; older records take the short path and default the
  new fields. Example: v3 added hyperedge-on-hyperedge role-fillers; a v2
  record decodes with `hyperedge_roles` empty.

### Executable contract

| Guarantee | Test |
|---|---|
| A v2-written record decodes on the current (v3) build, new fields empty | `record.rs::hyperedge_v2_byte_stream_decodes_with_empty_hyperedge_roles` |
| A future-version record is rejected, not mis-read | `record.rs::unsupported_format_version_rejected` |

These tests must never be deleted; they are the on-disk contract. When a
future `FORMAT_VERSION` (v4+) lands, add a "vN-byte-stream decodes on current
build" test alongside them — that is the price of bumping the format.

## Versioned surfaces (independent semver)

nDB versions four surfaces independently. A consumer pins to the ones it
depends on.

| Surface | Identifier | Where | Current | Stability rule |
|---|---|---|---|---|
| **Engine / server release** | `nDB X.Y.Z` | workspace `Cargo.toml` | `2.4.0` | SemVer. Breaking API/behaviour → major. |
| **Wire protocol** | `/vN` URL prefix | `ndb-server` routes | `v1` | Within `v1`: additive only. A break ships `/v2` *alongside* `/v1`; live clients never break. |
| **On-disk format** | `FORMAT_VERSION` | `record.rs` | `3` | "Always opens" (above). New version = new capability + retained old readers. |
| **TypeScript SDK** | `@ndb/client` semver | `clients/ts` | (initial) | SemVer, tracks protocol `v1`. Major SDK bump only on a protocol major. |

### What "pinning" means for a consumer

- **An application** pins the **SDK** (`@ndb/client@^1`) and talks to a `/v1`
  server. Any `1.x` server speaks `/v1`; upgrade the server freely.
- **A server operator** upgrades the **engine/server** release at will; the
  on-disk format promise means existing databases keep opening, and the `/v1`
  contract means existing clients keep working.
- **The on-disk format** is the longest-lived contract: data outlives both the
  server binary and the client. Hence the strongest guarantee lives there.

## Deprecation policy

- The pre-`/v1` **bare routes** (`/commit`, `/query`, …) remain as
  **deprecated aliases** of their `/v1` equivalents. They are not removed
  within the `v1` era; new integrations should target `/v1`.
- A deprecated surface is announced in `CHANGELOG`, kept for at least one
  engine major, and only removed in a major bump.

## Scope

This policy covers the engine, server, wire protocol, on-disk format, and the
TypeScript SDK. The Python client and CLI track the same wire protocol. Arrow
export follows the Apache Arrow IPC stability guarantees of the `arrow` crate
it depends on.
