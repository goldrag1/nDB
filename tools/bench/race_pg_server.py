#!/usr/bin/env python3
"""Live bench-race Postgres backend for the knowledge site.

Sibling of `crates/ndb-engine/examples/bench_race.rs`. Loads the same
realworld shape (50_000 entities + 50_000 hyperedges) into a persistent
Postgres database, then exposes a small HTTP/1.1 surface so the
`bench.html` page can POST `/run/<workload>` and watch live timings
race against nDB.

Surface (matches the nDB sibling byte-for-byte):
    GET  /health
    GET  /workloads
    POST /run/<name>

Database lifecycle:
    - On startup, connect to `postgresql:///postgres` (admin), check
      whether `BENCH_DB` exists. If yes, reuse it. If no, create + load.
    - Reuse keeps subsequent restarts cheap (~50 ms instead of ~1500 ms).
    - To force a fresh reload, set `DROP_FIRST=1` in env.

Read-only: same as the nDB side — no `commits_per_sec` workload.

Run with:
    python3 tools/bench/race_pg_server.py --bind 127.0.0.1:8772
"""

from __future__ import annotations
import argparse
import json
import os
import sys
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Lock

try:
    import psycopg
except ImportError:
    sys.exit("psycopg3 required: pip install --user --break-system-packages psycopg[binary]")

BENCH_DB = os.environ.get("BENCH_DB", "ndb_bench_race")
ADMIN_URL = os.environ.get("ADMIN_URL", "postgresql:///postgres")
DROP_FIRST = os.environ.get("DROP_FIRST") == "1"

N_CUSTOMERS = 49_000
N_REGIONS = 1_000
N_SALES = 45_000
N_CONTAINS = 5_000

# Iter counts — must keep total wall-clock per click ≤ ~500 ms so the
# live race feels snappy. Mirrors `bench_race.rs`.
N_ITER_LOOKUPS = 500
N_ITER_QUERY_SMALL = 50
N_ITER_QUERY_LARGE = 10  # two_pattern_join / count_aggregate
N_ITER_RECURSIVE = 25
N_ITER_ITERATE = 3

RATE_LIMIT_SECS = 3

WORKLOADS = [
    {"name": "iter_all", "label": "Full snapshot scan",
     "blurb": "Streaming walk of every entity + hyperedge.", "iters": N_ITER_ITERATE},
    {"name": "point_lookup", "label": "Random point lookup",
     "blurb": "Fetch one record by UUID, 500 times.", "iters": N_ITER_LOOKUPS},
    {"name": "property_lookup", "label": "Indexed property lookup",
     "blurb": "Look up customers by region code via the B-tree, 500 times.",
     "iters": N_ITER_LOOKUPS},
    {"name": "single_pattern_query", "label": "Single-pattern query",
     "blurb": "SELECT id FROM customer WHERE region_code = 'REG-00000'",
     "iters": N_ITER_QUERY_SMALL},
    {"name": "two_pattern_join", "label": "Two-pattern join",
     "blurb": "Customer × sales junction (HashJoin)",
     "iters": N_ITER_QUERY_LARGE},
    {"name": "recursive_contains_depth3", "label": "Recursive walk, depth 3",
     "blurb": "WITH RECURSIVE … depth ≤ 3",
     "iters": N_ITER_RECURSIVE},
    {"name": "count_aggregate", "label": "count() over a type",
     "blurb": "SELECT count(*) FROM customer  (49k rows)",
     "iters": N_ITER_QUERY_LARGE},
]


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def ensure_database() -> tuple[str, dict]:
    """Create or reuse the bench DB. Returns (connection-url, sample-data)."""
    if DROP_FIRST:
        with psycopg.connect(ADMIN_URL, autocommit=True) as admin:
            admin.execute(f"DROP DATABASE IF EXISTS {BENCH_DB}")

    with psycopg.connect(ADMIN_URL, autocommit=True) as admin:
        cur = admin.execute("SELECT 1 FROM pg_database WHERE datname=%s", (BENCH_DB,))
        exists = cur.fetchone() is not None
        if not exists:
            log(f"creating {BENCH_DB}...")
            admin.execute(f"CREATE DATABASE {BENCH_DB}")

    db_url = f"{ADMIN_URL.rsplit('/', 1)[0]}/{BENCH_DB}"

    if not exists:
        log("loading data...")
        t0 = time.perf_counter()
        with psycopg.connect(db_url) as conn:
            init_schema(conn)
            samples = load_data(conn)
            conn.execute("ANALYZE")
            conn.commit()
        log(f"loaded {N_CUSTOMERS + N_REGIONS} entities + "
            f"{N_SALES + N_CONTAINS} junction rows in {(time.perf_counter() - t0) * 1000:.0f} ms")
        return db_url, samples
    else:
        log(f"reusing existing {BENCH_DB}; sampling probes...")
        with psycopg.connect(db_url) as conn:
            samples = sample_existing(conn)
        log("samples ready")
        return db_url, samples


def init_schema(conn: psycopg.Connection) -> None:
    conn.execute("""
        CREATE TABLE region (
            id   uuid PRIMARY KEY,
            code text NOT NULL UNIQUE,
            name text NOT NULL
        )
    """)
    conn.execute("""
        CREATE TABLE customer (
            id          uuid PRIMARY KEY,
            name        text NOT NULL,
            region_code text NOT NULL
        )
    """)
    conn.execute("CREATE INDEX customer_region_idx ON customer (region_code)")
    conn.execute("""
        CREATE TABLE sales (
            id       uuid PRIMARY KEY,
            buyer_id uuid NOT NULL REFERENCES customer(id)
        )
    """)
    conn.execute("CREATE INDEX sales_buyer_idx ON sales (buyer_id)")
    conn.execute("""
        CREATE TABLE contains (
            id        uuid PRIMARY KEY,
            parent_id uuid NOT NULL REFERENCES region(id),
            child_id  uuid NOT NULL REFERENCES region(id)
        )
    """)
    conn.execute("CREATE INDEX contains_parent_idx ON contains (parent_id)")
    conn.commit()


def load_data(conn: psycopg.Connection) -> dict:
    region_codes, region_ids = [], []
    with conn.cursor() as cur:
        rows = []
        for i in range(N_REGIONS):
            rid = uuid.uuid4()
            code = f"REG-{i:05d}"
            rows.append((rid, code, f"Region {i}"))
            region_codes.append(code)
            region_ids.append(rid)
        cur.executemany("INSERT INTO region (id, code, name) VALUES (%s, %s, %s)", rows)
    conn.commit()

    cust_ids = []
    with conn.cursor() as cur:
        batch = []
        for i in range(N_CUSTOMERS):
            cid = uuid.uuid4()
            cust_ids.append(cid)
            batch.append((cid, f"Customer {i}", region_codes[i % N_REGIONS]))
            if len(batch) >= 5000:
                cur.executemany("INSERT INTO customer (id, name, region_code) VALUES (%s, %s, %s)", batch)
                batch = []
        if batch:
            cur.executemany("INSERT INTO customer (id, name, region_code) VALUES (%s, %s, %s)", batch)
    conn.commit()

    with conn.cursor() as cur:
        batch = []
        for i in range(N_SALES):
            if i % 2 == 0:
                cust_idx = (((i // 2) % ((N_CUSTOMERS + 19) // 20)) * 20) % N_CUSTOMERS
            else:
                cust_idx = (i * 31 + 7) % N_CUSTOMERS
            batch.append((uuid.uuid4(), cust_ids[cust_idx]))
            if len(batch) >= 5000:
                cur.executemany("INSERT INTO sales (id, buyer_id) VALUES (%s, %s)", batch)
                batch = []
        if batch:
            cur.executemany("INSERT INTO sales (id, buyer_id) VALUES (%s, %s)", batch)
    conn.commit()

    n = N_REGIONS
    with conn.cursor() as cur:
        batch = []
        for i in range(N_CONTAINS):
            parent_idx = i % n
            child_idx = (parent_idx + n // 4 + (i // n) * (n // 8)) % n
            if parent_idx == child_idx:
                continue
            batch.append((uuid.uuid4(), region_ids[parent_idx], region_ids[child_idx]))
            if len(batch) >= 5000:
                cur.executemany("INSERT INTO contains (id, parent_id, child_id) VALUES (%s, %s, %s)", batch)
                batch = []
        if batch:
            cur.executemany("INSERT INTO contains (id, parent_id, child_id) VALUES (%s, %s, %s)", batch)
    conn.commit()

    return _materialize_samples(region_codes, region_ids, cust_ids)


def sample_existing(conn: psycopg.Connection) -> dict:
    """Reuse: pull a deterministic sample of probes from the existing DB."""
    cur = conn.execute("SELECT code FROM region ORDER BY code LIMIT %s", (N_ITER_LOOKUPS,))
    codes = [r[0] for r in cur.fetchall()]
    cur = conn.execute("SELECT id FROM region ORDER BY code LIMIT 4")
    region_ids = [r[0] for r in cur.fetchall()]
    cur = conn.execute(
        "SELECT id FROM customer ORDER BY id LIMIT %s", (N_ITER_LOOKUPS,)
    )
    cust_ids = [r[0] for r in cur.fetchall()]
    return _materialize_samples(codes, region_ids, cust_ids)


def _materialize_samples(codes: list[str], region_ids: list, cust_ids: list) -> dict:
    import random
    rng = random.Random(0xdeadbeef)
    return {
        "narrow_region": "REG-00000",
        "lookup_ids": rng.choices(cust_ids, k=N_ITER_LOOKUPS),
        "lookup_codes": rng.choices(codes, k=N_ITER_LOOKUPS),
        "chain_root": region_ids[0],
    }


# ─── Workload runners ─────────────────────────────────────────────────

def finalize(name: str, samples_us: list[int], total_us: float) -> dict:
    samples_us.sort()
    n = len(samples_us)
    return {
        "name": name,
        "iters": n,
        "min_us": float(samples_us[0]),
        "p50_us": float(samples_us[n // 2]),
        "p99_us": float(samples_us[min(n - 1, n * 99 // 100)]),
        "ops_per_sec": (n * 1_000_000.0 / total_us) if total_us > 0 else 0.0,
        "total_ms": total_us / 1000.0,
    }


def run_workload(conn: psycopg.Connection, name: str, samples: dict) -> dict:
    if name == "iter_all":
        return _bench_iter_all(conn)
    if name == "point_lookup":
        return _bench_point_lookup(conn, samples["lookup_ids"])
    if name == "property_lookup":
        return _bench_property_lookup(conn, samples["lookup_codes"])
    if name == "single_pattern_query":
        return _bench_single_pattern(conn, samples["narrow_region"])
    if name == "two_pattern_join":
        return _bench_two_pattern_join(conn, samples["narrow_region"])
    if name == "recursive_contains_depth3":
        return _bench_recursive(conn, samples["chain_root"])
    if name == "count_aggregate":
        return _bench_count_aggregate(conn)
    raise ValueError(f"unknown workload: {name}")


def _bench_iter_all(conn: psycopg.Connection) -> dict:
    samples, outer = [], time.perf_counter()
    for _ in range(N_ITER_ITERATE):
        t = time.perf_counter()
        n = 0
        with conn.cursor("iter") as cur:
            cur.itersize = 5000
            cur.execute(
                "SELECT id FROM region UNION ALL SELECT id FROM customer UNION ALL "
                "SELECT id FROM sales UNION ALL SELECT id FROM contains"
            )
            for _ in cur:
                n += 1
        samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("iter_all", samples, (time.perf_counter() - outer) * 1e6)


def _bench_point_lookup(conn: psycopg.Connection, ids: list) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for cid in ids:
            t = time.perf_counter()
            cur.execute("SELECT id, name, region_code FROM customer WHERE id = %s", (cid,))
            cur.fetchone()
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("point_lookup", samples, (time.perf_counter() - outer) * 1e6)


def _bench_property_lookup(conn: psycopg.Connection, codes: list[str]) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for code in codes:
            t = time.perf_counter()
            cur.execute("SELECT id FROM customer WHERE region_code = %s", (code,))
            cur.fetchall()
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("property_lookup", samples, (time.perf_counter() - outer) * 1e6)


def _bench_single_pattern(conn: psycopg.Connection, region: str) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for _ in range(N_ITER_QUERY_SMALL):
            t = time.perf_counter()
            cur.execute("SELECT id FROM customer WHERE region_code = %s", (region,))
            cur.fetchall()
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("single_pattern_query", samples, (time.perf_counter() - outer) * 1e6)


def _bench_two_pattern_join(conn: psycopg.Connection, region: str) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for _ in range(N_ITER_QUERY_LARGE):
            t = time.perf_counter()
            cur.execute("""
                SELECT c.id, s.id
                  FROM customer c
                  JOIN sales s ON s.buyer_id = c.id
                 WHERE c.region_code = %s
            """, (region,))
            cur.fetchall()
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("two_pattern_join", samples, (time.perf_counter() - outer) * 1e6)


def _bench_recursive(conn: psycopg.Connection, root) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for _ in range(N_ITER_RECURSIVE):
            t = time.perf_counter()
            cur.execute("""
                WITH RECURSIVE walk(node, depth) AS (
                    SELECT child_id, 1 FROM contains WHERE parent_id = %s
                    UNION
                    SELECT c.child_id, w.depth + 1
                      FROM contains c JOIN walk w ON c.parent_id = w.node
                     WHERE w.depth < 3
                )
                SELECT DISTINCT node FROM walk
            """, (root,))
            cur.fetchall()
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("recursive_contains_depth3", samples, (time.perf_counter() - outer) * 1e6)


def _bench_count_aggregate(conn: psycopg.Connection) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for _ in range(N_ITER_QUERY_LARGE):
            t = time.perf_counter()
            cur.execute("SELECT count(*) FROM customer")
            cur.fetchone()
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("count_aggregate", samples, (time.perf_counter() - outer) * 1e6)


# ─── HTTP server ──────────────────────────────────────────────────────

class State:
    def __init__(self, db_url: str, samples: dict, load_ms: float):
        self.db_url = db_url
        self.samples = samples
        self.load_ms = load_ms
        self.conn_lock = Lock()
        self.conn = psycopg.connect(db_url)
        self.rate_limit: dict[tuple[str, str], float] = {}
        self.rate_lock = Lock()


def make_handler(state: State):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):  # silence default access log
            return

        def _send_json(self, status: int, body: dict | list):
            data = json.dumps(body).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            if self.path == "/health":
                self._send_json(200, {
                    "status": "ok", "loaded": True,
                    "engine": f"postgres {sys.version.split()[0]}-py",
                    "n_entities": N_CUSTOMERS + N_REGIONS,
                    "n_hyperedges": N_SALES + N_CONTAINS,
                    "load_ms": state.load_ms,
                })
                return
            if self.path == "/workloads":
                self._send_json(200, WORKLOADS)
                return
            self._send_json(404, {"error": "not_found"})

        def do_POST(self):
            if not self.path.startswith("/run/"):
                self._send_json(404, {"error": "not_found"})
                return
            name = self.path[len("/run/"):]
            workload = next((w for w in WORKLOADS if w["name"] == name), None)
            if workload is None:
                self._send_json(404, {"error": "unknown_workload"})
                return
            # Rate limit.
            peer = self.client_address[0] if self.client_address else "?"
            key = (peer, name)
            now = time.monotonic()
            with state.rate_lock:
                prev = state.rate_limit.get(key)
                if prev is not None and now - prev < RATE_LIMIT_SECS:
                    wait = int(RATE_LIMIT_SECS - (now - prev))
                    self._send_json(429, {"error": "rate_limit", "retry_after_s": wait})
                    return
                state.rate_limit[key] = now
                if len(state.rate_limit) > 256:
                    cutoff = now - 60
                    state.rate_limit = {k: v for k, v in state.rate_limit.items() if v > cutoff}
            # Run (serialized via conn_lock so a single PG connection is shared).
            with state.conn_lock:
                try:
                    out = run_workload(state.conn, name, state.samples)
                except Exception as e:
                    log(f"workload {name} failed: {e}")
                    # Reset connection.
                    try:
                        state.conn.close()
                    except Exception:
                        pass
                    state.conn = psycopg.connect(state.db_url)
                    self._send_json(500, {"error": "workload_failed", "detail": str(e)})
                    return
            self._send_json(200, out)

    return Handler


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bind", default="127.0.0.1:8772")
    args = ap.parse_args()

    t0 = time.perf_counter()
    db_url, samples = ensure_database()
    load_ms = (time.perf_counter() - t0) * 1000.0
    state = State(db_url, samples, load_ms)

    host, port = args.bind.rsplit(":", 1)
    server = ThreadingHTTPServer((host, int(port)), make_handler(state))
    log(f"bench-race PG serving on http://{host}:{port}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        log("shutdown")
    return 0


if __name__ == "__main__":
    sys.exit(main())
