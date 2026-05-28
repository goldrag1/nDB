#!/usr/bin/env bash
# Run the realworld micro-benchmark for nDB + Postgres, merge results,
# and print a markdown report. Requires:
#   - cargo (for the Rust bench)
#   - python3 + psycopg3 (for the PG bench)
#   - A reachable Postgres on the local socket OR a DATABASE_URL env var
#
# Output:
#   docs/benchmarks/realworld-$(date +%F).md  (markdown report)
#   docs/benchmarks/realworld-$(date +%F)-ndb.json
#   docs/benchmarks/realworld-$(date +%F)-pg.json
#
# Usage:
#   tools/bench/run_realworld.sh          # both engines
#   SKIP_PG=1 tools/bench/run_realworld.sh # nDB only

set -e
cd "$(dirname "$0")/../.."

DATE=$(date +%F)
NDB_JSON=docs/benchmarks/realworld-$DATE-ndb.json
PG_JSON=docs/benchmarks/realworld-$DATE-pg.json
REPORT=docs/benchmarks/realworld-$DATE.md
mkdir -p docs/benchmarks

echo ">> Building Rust bench (release)..."
cargo build --release --example realworld_bench -p ndb-engine 2>&1 | tail -3

echo ">> Running nDB bench..."
./target/release/examples/realworld_bench > "$NDB_JSON" 2>&1 | grep -E '^(→|←|loaded|DB:|bytes_|\|)' || true

if [ -z "${SKIP_PG:-}" ]; then
    echo ">> Running Postgres bench..."
    python3 tools/bench/realworld_pg.py > "$PG_JSON" 2>&1 | grep -E '^(loading|loaded|\||tmp|bytes_)' || true
    echo ">> Rendering merged report → $REPORT"
    python3 tools/bench/render_realworld.py \
        --ndb "$NDB_JSON" --pg "$PG_JSON" \
        --title "Real-world micro-benchmark — nDB v1.3 vs PostgreSQL ($DATE)" \
        > "$REPORT"
else
    echo ">> Rendering nDB-only report → $REPORT"
    python3 tools/bench/render_realworld.py \
        --ndb "$NDB_JSON" \
        --title "Real-world micro-benchmark — nDB v1.3 ($DATE)" \
        > "$REPORT"
fi

echo
echo ">> Done."
echo ">> Report: $REPORT"
echo ">> Raw:    $NDB_JSON ${PG_JSON:+, $PG_JSON}"
