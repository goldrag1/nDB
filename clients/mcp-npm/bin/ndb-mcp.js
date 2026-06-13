#!/usr/bin/env node
/**
 * @n-dimension-database-ndb/mcp launcher.
 *
 * Resolves the platform `ndb-mcp-server` binary and execs it, passing through
 * args and stdio so the MCP JSON-RPC transport (newline-delimited over
 * stdin/stdout) flows straight between the agent and the binary.
 *
 * Resolution order:
 *   1. $NDB_MCP_SERVER_BIN                      (explicit override)
 *   2. the matching per-platform npm package    (optionalDependencies; prod)
 *   3. a local cargo build under target/         (dev checkout)
 *   4. `ndb-mcp-server` on PATH                  (last resort)
 */
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));

/** npm package + binary name for the current platform/arch. */
function platformTarget() {
  const plat = process.platform; // 'linux' | 'darwin' | 'win32'
  const arch = process.arch; // 'x64' | 'arm64'
  const map = {
    "linux-x64": "linux-x64",
    "linux-arm64": "linux-arm64",
    "darwin-arm64": "darwin-arm64",
  };
  const key = map[`${plat}-${arch}`];
  const ext = plat === "win32" ? ".exe" : "";
  return { pkg: key && `@n-dimension-database-ndb/mcp-${key}`, ext };
}

function resolveBinary() {
  const { pkg, ext } = platformTarget();
  const binName = `ndb-mcp-server${ext}`;

  // 1. explicit override
  const override = process.env.NDB_MCP_SERVER_BIN;
  if (override) {
    if (!existsSync(override)) {
      fail(`NDB_MCP_SERVER_BIN points at a missing file: ${override}`);
    }
    return override;
  }

  // 2. per-platform npm package
  if (pkg) {
    try {
      return require.resolve(`${pkg}/bin/${binName}`);
    } catch {
      /* not installed (e.g. unsupported platform or dev checkout) — fall through */
    }
  }

  // 3. local cargo build (dev): walk up looking for target/{release,debug}
  let dir = here;
  for (let i = 0; i < 8; i++) {
    for (const profile of ["release", "debug"]) {
      const cand = join(dir, "target", profile, binName);
      if (existsSync(cand)) return cand;
    }
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }

  // 4. PATH
  return binName;
}

function fail(msg) {
  process.stderr.write(`@n-dimension-database-ndb/mcp: ${msg}\n`);
  process.exit(1);
}

const bin = resolveBinary();
const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });

child.on("error", (e) => {
  fail(
    `failed to launch ndb-mcp-server (${bin}): ${e.message}\n` +
      `Install a release, set NDB_MCP_SERVER_BIN, or 'cargo build -p ndb-mcp-server'.`,
  );
});
child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 0);
});

// Forward termination signals so the agent can stop the server cleanly.
for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, () => child.kill(sig));
}
