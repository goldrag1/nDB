#!/usr/bin/env bash
# Supervisor for the langgraph view server (gap 4: ops / no-crash).
#
# Builds the server once, then runs it in a restart loop with health
# logging. If the process dies, it comes back in 2s; meanwhile the site's
# /langgraph_ndb/api proxy returns 502 and the explorer falls back to the
# static graph.json — so a server crash degrades, it never takes the demo
# down.
#
# Dev:  tools/langgraph/serve.sh [DB_DIR] [BIND]
# Prod: run under systemd instead (Restart=always), e.g.
#   [Service]
#   ExecStart=/opt/ndb/target/release/langgraph-server \
#       --db /opt/ndb/.demo-data/langgraph-ndb --bind 127.0.0.1:8791
#   Restart=always
#   RestartSec=2
# and let the site reverse-proxy /langgraph_ndb/api → 127.0.0.1:8791.
# Health probe: GET /health returns {"status":"ok","papers":N}.
set -u

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DB="${1:-$ROOT/.demo-data/langgraph-ndb}"
BIND="${2:-127.0.0.1:8791}"

cd "$ROOT" || exit 1
echo "[serve] building langgraph-server…"
cargo build --release -p langgraph --bin langgraph-server || { echo "[serve] build failed"; exit 1; }

while true; do
  echo "[serve] $(date -Is) starting langgraph-server --db $DB --bind $BIND"
  ./target/release/langgraph-server --db "$DB" --bind "$BIND"
  code=$?
  echo "[serve] $(date -Is) exited (code $code) — restarting in 2s"
  sleep 2
done
