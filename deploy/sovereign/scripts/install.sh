#!/usr/bin/env bash
# Install the nDB sovereign stack (data API + MCP HTTP + Pingora TLS edge) as
# three cgroup-capped systemd services. Run as root on the target VPS, from the
# repo root, AFTER you've built the binaries.
#
#   # 1. build (on the box, or scp prebuilt binaries to /usr/local/bin)
#   cargo build --release -p ndb-server -p ndb-mcp-server
#   ( cd deploy/sovereign/ndb-edge && cargo build --release )
#
#   # 2. install
#   sudo deploy/sovereign/scripts/install.sh
#
#   # 3. certs + token, then start
#   sudo deploy/sovereign/scripts/obtain-certs.sh ndb.example.com you@example.com
#   sudo sed -i "s/replace-with-openssl-rand-hex-32/$(openssl rand -hex 32)/" /etc/ndb/ndb.env
#   sudo systemctl enable --now ndb-server ndb-mcp ndb-edge
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
SRC="${REPO_ROOT}/deploy/sovereign"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }

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

# env file (don't clobber an existing one)
[ -f /etc/ndb/ndb.env ] || { install -m 0600 "${SRC}/ndb.env.example" /etc/ndb/ndb.env; echo "wrote /etc/ndb/ndb.env (EDIT IT: set NDB_TOKEN)"; }

# units
install -m 0644 "${SRC}/systemd/"*.service /etc/systemd/system/
systemctl daemon-reload

echo "installed. next: obtain-certs.sh, set NDB_TOKEN in /etc/ndb/ndb.env, then:"
echo "  systemctl enable --now ndb-server ndb-mcp ndb-edge"
