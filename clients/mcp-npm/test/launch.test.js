/**
 * Launch test: the @ndb/mcp wrapper resolves a binary, starts the server, and
 * speaks MCP (newline-delimited JSON-RPC) over stdio — initialize + tools/list
 * return the nDB tool surface.
 *
 * Requires a built MCP server. Defaults to target/debug/ndb-mcp-server at the
 * repo root; override with NDB_MCP_SERVER_BIN. Build: cargo build -p ndb-mcp-server.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const LAUNCHER = resolve(here, "../bin/ndb-mcp.js");
const DEFAULT_BIN = resolve(here, "../../../target/debug/ndb-mcp-server");

test("launcher starts the server and lists nDB tools over MCP", async () => {
  const bin = process.env.NDB_MCP_SERVER_BIN ?? DEFAULT_BIN;
  if (!existsSync(bin)) {
    throw new Error(`missing ${bin} — build it: cargo build -p ndb-mcp-server`);
  }
  const db = mkdtempSync(join(tmpdir(), "ndb-mcp-it-"));
  const child = spawn(process.execPath, [LAUNCHER, "--path", db], {
    stdio: ["pipe", "pipe", "inherit"],
    env: { ...process.env, NDB_MCP_SERVER_BIN: bin },
  });

  const tools = await new Promise((resolveP, rejectP) => {
    let buf = "";
    const timer = setTimeout(() => rejectP(new Error("timeout")), 10_000);
    child.stdout.on("data", (chunk) => {
      buf += chunk.toString();
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (!line) continue;
        let msg;
        try {
          msg = JSON.parse(line);
        } catch {
          continue;
        }
        if (msg.id === 2) {
          clearTimeout(timer);
          resolveP(msg.result?.tools ?? []);
        }
      }
    });
    child.on("error", rejectP);
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize" })}\n`);
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/list" })}\n`);
  }).finally(() => child.kill("SIGKILL"));

  rmSync(db, { recursive: true, force: true });

  assert.ok(Array.isArray(tools) && tools.length > 0, "tools/list returns tools");
  const names = tools.map((t) => t.name);
  assert.ok(
    names.some((n) => String(n).includes("commit_hyperedge")),
    `expected an ndb hyperedge tool; got: ${names.join(", ")}`,
  );
});
