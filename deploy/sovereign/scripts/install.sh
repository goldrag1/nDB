#!/usr/bin/env bash
# nDB one-command installer.
#
# Two modes:
#
#   # A) Local trial — zero → first query, no root, no systemd, no certs.
#   #    Builds (if needed) and runs the UNIFIED server: /mcp on :9000 and the
#   #    /v1 data API on :8742, one process, one engine.
#   deploy/sovereign/scripts/install.sh --local
#
#   # B) Production sovereign stack — cgroup-capped systemd services behind a
#   #    Pingora TLS edge. Run as root from the repo root. Give a domain+email
#   #    and it chains build → install → certs → token → start in one command:
#   sudo deploy/sovereign/scripts/install.sh ndb.example.com you@example.com
#
#   #    …or just install the units and do certs/token/start yourself:
#   sudo deploy/sovereign/scripts/install.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
SRC="${REPO_ROOT}/deploy/sovereign"

# --------------------------------------------------------------------------
# A) Local trial — fastest path to a running, queryable nDB.
# --------------------------------------------------------------------------
if [ "${1:-}" = "--local" ]; then
  DATA="${NDB_LOCAL_DATA:-${REPO_ROOT}/.ndb-local}"
  MCP_ADDR="${NDB_LOCAL_MCP:-127.0.0.1:9000}"
  V1_ADDR="${NDB_LOCAL_V1:-127.0.0.1:8742}"
  BIN="${REPO_ROOT}/target/release/ndb-mcp-server"

  if [ ! -x "$BIN" ]; then
    echo "building ndb-mcp-server (release)…"
    ( cd "$REPO_ROOT" && cargo build --release -p ndb-mcp-server )
  fi
  mkdir -p "$DATA"

  echo "starting unified nDB: /mcp -> http://${MCP_ADDR}/mcp   /v1 -> http://${V1_ADDR}/v1"
  "$BIN" --path "$DATA" --http "$MCP_ADDR" --v1-bind "$V1_ADDR" &
  PID=$!
  trap 'kill "$PID" 2>/dev/null || true' EXIT

  # Wait for the data API to answer (≤10s).
  for _ in $(seq 1 50); do
    if curl -fs "http://${V1_ADDR}/v1/health" >/dev/null 2>&1; then break; fi
    sleep 0.2
  done

  echo
  echo "✅ nDB is up. Your first query:"
  echo "   curl http://${V1_ADDR}/v1/health"
  curl -s "http://${V1_ADDR}/v1/health" && echo
  echo
  echo "Python (pip install 'ndb-client[arrow]'):"
  echo "   from ndb_client import Client"
  echo "   db = Client('http://${V1_ADDR}'); print(db.health())"
  echo
  echo "MCP endpoint for agents: http://${MCP_ADDR}/mcp   (tools/list, tools/call)"
  echo "data dir: ${DATA}   |   stop: Ctrl-C (this script foregrounds the server)"
  echo
  wait "$PID"
  exit 0
fi

# --------------------------------------------------------------------------
# B) Production install (systemd + Pingora edge). Needs root.
# --------------------------------------------------------------------------
[ "$(id -u)" -eq 0 ] || { echo "run as root (or use --local for a no-root trial)"; exit 1; }

DOMAIN="${1:-}"
EMAIL="${2:-}"

# Build any missing binaries so a bare checkout still works in one command.
need_build=0
for bin in ndb-mcp-server ndb-server; do
  [ -x "${REPO_ROOT}/target/release/${bin}" ] || need_build=1
done
[ -x "${SRC}/ndb-edge/target/release/ndb-edge" ] || need_build=1
if [ "$need_build" -eq 1 ]; then
  echo "building release binaries (this can take a few minutes)…"
  ( cd "$REPO_ROOT" && cargo build --release -p ndb-server -p ndb-mcp-server )
  ( cd "${SRC}/ndb-edge" && cargo build --release )
fi

# service user + data dirs
id ndb >/dev/null 2>&1 || useradd --system --home /var/lib/ndb --shell /usr/sbin/nologin ndb
install -d -o ndb -g ndb -m 0750 /var/lib/ndb /var/lib/ndb/data /var/lib/ndb/mcp-data
install -d -m 0755 /etc/ndb

# binaries (prefer release; fall back to debug for a quick trial)
for bin in ndb-server ndb-mcp-server; do
  if   [ -x "${REPO_ROOT}/target/release/${bin}" ]; then install -m 0755 "${REPO_ROOT}/target/release/${bin}" /usr/local/bin/
  elif [ -x "${REPO_ROOT}/target/debug/${bin}" ];   then install -m 0755 "${REPO_ROOT}/target/debug/${bin}" /usr/local/bin/
  else echo "missing ${bin} — build it first"; exit 1; fi
done
if   [ -x "${SRC}/ndb-edge/target/release/ndb-edge" ]; then install -m 0755 "${SRC}/ndb-edge/target/release/ndb-edge" /usr/local/bin/
else echo "WARNING: ndb-edge not built (cd deploy/sovereign/ndb-edge && cargo build --release). Skipping edge binary."; fi

# config: ndb.conf is the single source of truth a dev edits; ndb-configure
# renders it into ndb.env + systemd cgroup drop-ins (caps can't be env-expanded).
[ -f /etc/ndb/ndb.conf ] || { install -m 0600 "${SRC}/ndb.conf.example" /etc/ndb/ndb.conf; echo "wrote /etc/ndb/ndb.conf (edit this to tune everything)"; }
install -m 0755 "${SRC}/scripts/ndb-configure" /usr/local/bin/ndb-configure

# units (baseline; ndb-configure drop-ins override the caps from ndb.conf)
install -m 0644 "${SRC}/systemd/"*.service /etc/systemd/system/
systemctl daemon-reload

# One-command production: domain + email → certs, token, render-config, start.
if [ -n "$DOMAIN" ] && [ -n "$EMAIL" ]; then
  echo "obtaining TLS cert for ${DOMAIN}…"
  "${SRC}/scripts/obtain-certs.sh" "$DOMAIN" "$EMAIL"
  if grep -q "replace-with-openssl-rand-hex-32" /etc/ndb/ndb.conf; then
    sed -i "s/replace-with-openssl-rand-hex-32/$(openssl rand -hex 32)/" /etc/ndb/ndb.conf
    echo "set a fresh NDB_TOKEN in /etc/ndb/ndb.conf"
  fi
  systemctl enable ndb-server ndb-mcp ndb-edge
  ndb-configure                       # render ndb.env + cgroup drop-ins, start + verify
  echo
  echo "✅ live. verify:  curl -s https://${DOMAIN}/health"
  echo "token (devs need it):  grep NDB_TOKEN /etc/ndb/ndb.conf"
  echo "tune anything later: edit /etc/ndb/ndb.conf then  sudo ndb-configure"
else
  echo "installed. next (or re-run with 'DOMAIN EMAIL' to do these automatically):"
  echo "  ${SRC}/scripts/obtain-certs.sh ndb.example.com you@example.com"
  echo "  sed -i \"s/replace-with-openssl-rand-hex-32/\$(openssl rand -hex 32)/\" /etc/ndb/ndb.conf"
  echo "  systemctl enable ndb-server ndb-mcp ndb-edge"
  echo "  sudo ndb-configure          # renders /etc/ndb/ndb.conf -> env + cgroup caps, starts services"
fi
