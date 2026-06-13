/**
 * Integration test: drives the real `ndb-server` over HTTP via the SDK.
 *
 * Run: `node --experimental-strip-types --test test/` (the package `test`
 * script). Requires a built server binary; set `NDB_SERVER_BIN` or it
 * defaults to `../../target/debug/ndb-server`. Build it first with
 * `cargo build -p ndb-server`.
 */
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

import { NdbClient, NdbError } from "../src/index.ts";

const here = dirname(fileURLToPath(import.meta.url));
const SERVER_BIN =
  process.env.NDB_SERVER_BIN ??
  resolve(here, "../../../target/debug/ndb-server");
const PORT = 8761;
const BASE = `http://127.0.0.1:${PORT}`;

let server: ChildProcess;
let dbDir: string;
const client = new NdbClient(BASE, { retries: 3, baseBackoffMs: 50 });

async function waitForHealth(timeoutMs = 15_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const h = await client.health();
      if (h.status === "ok") return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error("server did not become healthy in time");
}

before(async () => {
  dbDir = mkdtempSync(join(tmpdir(), "ndb-ts-it-"));
  server = spawn(SERVER_BIN, ["--path", dbDir, "--bind", `127.0.0.1:${PORT}`], {
    stdio: "ignore",
  });
  server.on("error", (e) => {
    throw new Error(
      `failed to spawn ${SERVER_BIN}: ${e.message} (build it: cargo build -p ndb-server)`,
    );
  });
  await waitForHealth();
});

after(() => {
  server?.kill("SIGKILL");
  if (dbDir) rmSync(dbDir, { recursive: true, force: true });
});

test("health over /v1", async () => {
  const h = await client.health();
  assert.equal(h.status, "ok");
});

test("commit → read round-trip over /v1", async () => {
  const id = randomUUID();
  const c = await client.commit({
    records: [
      {
        kind: "entity",
        entity_id: id,
        type_id: 1,
        tx_id_assert: 0,
        tx_id_supersede: "active",
        properties: [
          { prop_id: 10, value: { tag: "string", value: "alice@example.com" } },
        ],
      },
    ],
  });
  assert.ok(c.tx_id > 0, "tx_id should be assigned");

  const r = await client.read(id);
  assert.equal(r.outcome, "live");
});

test("iter returns the committed record", async () => {
  const records = await client.iter();
  assert.ok(Array.isArray(records), "iter returns an array (parsed JSONL)");
  assert.ok(records.length >= 1, "at least the committed entity is present");
});

test("reading an unknown id is not-found, not a throw", async () => {
  const r = await client.read(randomUUID());
  assert.notEqual(r.outcome, "live");
});

test("NdbError surfaces protocol errors with status + parsed code", async () => {
  // A malformed query body must produce a structured NdbError whose code comes
  // from the server envelope ({error, detail}), not the generic fallback.
  await assert.rejects(
    () => client.query("this is not a valid QueryRequest object" as unknown),
    (err: unknown) =>
      err instanceof NdbError &&
      err.status >= 400 &&
      err.code !== "http_error" && // proves the {error, detail} envelope was parsed
      err.code.length > 0,
  );
});
