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
try:
    from psycopg_pool import ConnectionPool
except ImportError:
    sys.exit("psycopg_pool required: pip install --user --break-system-packages psycopg_pool")

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
        # Lazy pool for stress mode — sized up to the max stress
        # concurrency. Created on first /stress request so non-stress
        # users don't pay for it.
        self.pool: ConnectionPool | None = None
        self.pool_lock = Lock()

    def get_pool(self, min_size: int) -> ConnectionPool:
        with self.pool_lock:
            if self.pool is None:
                self.pool = ConnectionPool(
                    self.db_url,
                    min_size=4, max_size=128,
                    timeout=10, num_workers=2,
                )
                self.pool.wait()
            return self.pool


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
            if self.path == "/stats":
                self._send_json(200, collect_stats(state))
                return
            self._send_json(404, {"error": "not_found"})

        def do_POST(self):
            if self.path == "/stress":
                self._do_stress()
                return
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

        def _do_stress(self):
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            try:
                req = json.loads(raw or b"{}")
            except json.JSONDecodeError:
                self._send_json(400, {"error": "bad_json"})
                return
            workload_name = req.get("workload", "")
            workload = next((w for w in WORKLOADS if w["name"] == workload_name), None)
            if workload is None:
                self._send_json(404, {"error": "unknown_workload"})
                return
            concurrency = max(1, min(128, int(req.get("concurrency", 4))))
            duration_ms = max(500, min(30_000, int(req.get("duration_ms", 5000))))
            # Rate limit.
            peer = self.client_address[0] if self.client_address else "?"
            key = (peer, "/stress")
            now = time.monotonic()
            with state.rate_lock:
                prev = state.rate_limit.get(key)
                if prev is not None and now - prev < RATE_LIMIT_SECS:
                    wait = int(RATE_LIMIT_SECS - (now - prev))
                    self._send_json(429, {"error": "rate_limit", "retry_after_s": wait})
                    return
                state.rate_limit[key] = now
            try:
                out = run_stress(state, workload_name, concurrency, duration_ms)
            except Exception as e:
                log(f"stress {workload_name} failed: {e}")
                self._send_json(500, {"error": "stress_failed", "detail": str(e)})
                return
            self._send_json(200, out)

    return Handler


# ─── Concurrent stress runner ─────────────────────────────────────────

def run_stress(state: "State", workload_name: str, concurrency: int, duration_ms: int) -> dict:
    """Fire `concurrency` worker threads, each holding its own pooled
    connection, looping the workload until deadline. Merge per-thread
    latencies, compute percentiles + a log10 histogram, return."""
    import threading
    pool = state.get_pool(concurrency)
    deadline = time.monotonic() + (duration_ms / 1000.0)
    started = time.perf_counter()
    samples = state.samples

    latencies: list[list[int]] = [[] for _ in range(concurrency)]
    errors = [0] * concurrency

    def worker(tid: int) -> None:
        idx_seed = (tid * 0x9e3779b97f4a7c15) & 0xFFFFFFFFFFFFFFFF
        while time.monotonic() < deadline:
            idx_seed ^= (idx_seed << 13) & 0xFFFFFFFFFFFFFFFF
            idx_seed ^= idx_seed >> 7
            idx_seed ^= (idx_seed << 17) & 0xFFFFFFFFFFFFFFFF
            idx = idx_seed
            t0 = time.perf_counter()
            try:
                with pool.connection() as conn:
                    _do_one_op(conn, workload_name, samples, idx)
                latencies[tid].append(int((time.perf_counter() - t0) * 1e6))
            except Exception:
                errors[tid] += 1

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(concurrency)]
    for t in threads: t.start()
    for t in threads: t.join()
    wall_ms = (time.perf_counter() - started) * 1000.0

    flat: list[int] = []
    for lst in latencies:
        flat.extend(lst)
    flat.sort()
    total_ops = len(flat)
    err_total = sum(errors)

    def pct(q: float) -> int:
        if not flat: return 0
        return flat[min(len(flat) - 1, int(len(flat) * q))]

    p50 = pct(0.50); p95 = pct(0.95); p99 = pct(0.99); p999 = pct(0.999)
    rps = (total_ops * 1000.0 / wall_ms) if wall_ms > 0 else 0.0

    # Log10 histogram: 6 decades × 10 bins per decade = 60 bins.
    import math
    hist = [0] * 60
    for us in flat:
        v = max(1, us)
        b = int(math.log10(v) * 10)
        if b < 0: b = 0
        if b >= 60: b = 59
        hist[b] += 1
    hist_pairs = []
    for i, count in enumerate(hist):
        if count == 0: continue
        edge = 10 ** (i / 10.0)
        hist_pairs.append([round(edge), count])

    return {
        "workload": workload_name,
        "concurrency": concurrency,
        "duration_ms": duration_ms,
        "wall_ms": wall_ms,
        "total_ops": total_ops,
        "errors": err_total,
        "rps": rps,
        "p50_us": p50, "p95_us": p95, "p99_us": p99, "p999_us": p999,
        "max_us": flat[-1] if flat else 0,
        "histogram_log10": hist_pairs,
    }


def collect_stats(state: "State") -> dict:
    """Snapshot disk + RAM + CPU for the bench Postgres. Bytes on disk
    comes from pg_database_size(); RSS + CPU are summed across every
    `postgres` process whose cmdline contains the bench DB name (covers
    every active and idle pooled backend, not just the ones currently
    showing up in pg_stat_activity). Cheap (~5 ms)."""
    bytes_on_disk = 0
    try:
        with state.conn_lock:
            cur = state.conn.execute("SELECT pg_database_size(current_database())")
            row = cur.fetchone()
            bytes_on_disk = int(row[0]) if row else 0
    except Exception:
        pass
    backend_pids = _find_pg_backends(BENCH_DB)
    rss_kb = 0
    cpu_us = 0
    for pid in backend_pids:
        rss_kb += _proc_rss_kb(pid)
        cpu_us += _proc_cpu_us(pid)
    return {
        "bytes_on_disk": bytes_on_disk,
        "bytes_resident": rss_kb * 1024,
        "cpu_user_us": cpu_us,  # we don't split user/sys; sum is what the lane cares about
        "cpu_sys_us": 0,
        "backend_pids": len(backend_pids),
    }


def _find_pg_backends(db_name: str) -> list[int]:
    """Walk /proc and return PIDs of postgres processes whose argv0
    cmdline contains the bench DB name. `postgres: 16/main: long
    ndb_bench_race [local] idle` etc. — matches both active and idle
    pooled backends."""
    pids = []
    try:
        for entry in os.listdir("/proc"):
            if not entry.isdigit(): continue
            try:
                with open(f"/proc/{entry}/cmdline", "rb") as f:
                    raw = f.read().decode("utf-8", errors="ignore")
                if "postgres" in raw and db_name in raw:
                    pids.append(int(entry))
            except OSError:
                continue
    except OSError:
        pass
    return pids


def _proc_rss_kb(pid: int) -> int:
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    parts = line.split()
                    return int(parts[1])
    except (OSError, ValueError):
        pass
    return 0


def _proc_cpu_us(pid: int) -> int:
    """Sum (utime + stime) for one process, in microseconds (assumes
    100 Hz clock — standard on every Linux distro)."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            raw = f.read()
        # comm field can contain spaces; skip past the closing ')'.
        rest = raw.rsplit(") ", 1)[1]
        parts = rest.split()
        utime = int(parts[11])
        stime = int(parts[12])
        return (utime + stime) * 10_000  # 10_000 μs per tick at 100 Hz
    except (OSError, ValueError, IndexError):
        return 0


def _do_one_op(conn: psycopg.Connection, workload_name: str, samples: dict, idx: int) -> None:
    """One iteration of the named workload — fastest path possible."""
    with conn.cursor() as cur:
        if workload_name == "iter_all":
            cur.execute(
                "SELECT id FROM region UNION ALL SELECT id FROM customer UNION ALL "
                "SELECT id FROM sales UNION ALL SELECT id FROM contains"
            )
            for _ in cur: pass
        elif workload_name == "point_lookup":
            pool = samples["lookup_ids"]
            cur.execute("SELECT id, name, region_code FROM customer WHERE id = %s",
                        (pool[idx % len(pool)],))
            cur.fetchone()
        elif workload_name == "property_lookup":
            pool = samples["lookup_codes"]
            cur.execute("SELECT id FROM customer WHERE region_code = %s",
                        (pool[idx % len(pool)],))
            cur.fetchall()
        elif workload_name == "single_pattern_query":
            cur.execute("SELECT id FROM customer WHERE region_code = %s",
                        (samples["narrow_region"],))
            cur.fetchall()
        elif workload_name == "two_pattern_join":
            cur.execute("""
                SELECT c.id, s.id
                  FROM customer c
                  JOIN sales s ON s.buyer_id = c.id
                 WHERE c.region_code = %s
            """, (samples["narrow_region"],))
            cur.fetchall()
        elif workload_name == "recursive_contains_depth3":
            cur.execute("""
                WITH RECURSIVE walk(node, depth) AS (
                    SELECT child_id, 1 FROM contains WHERE parent_id = %s
                    UNION
                    SELECT c.child_id, w.depth + 1
                      FROM contains c JOIN walk w ON c.parent_id = w.node
                     WHERE w.depth < 3
                )
                SELECT DISTINCT node FROM walk
            """, (samples["chain_root"],))
            cur.fetchall()
        elif workload_name == "count_aggregate":
            cur.execute("SELECT count(*) FROM customer")
            cur.fetchone()


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
