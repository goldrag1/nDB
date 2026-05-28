#!/usr/bin/env python3
"""Live bench-race SQLite backend for the knowledge site.

Sibling of `crates/ndb-engine/examples/bench_race.rs` and of
`tools/bench/race_pg_server.py`. Loads the same realworld shape
(50_000 entities + 50_000 hyperedges) into a persistent SQLite
database file, then exposes a small HTTP/1.1 surface so the
`bench.html` page can POST `/run/<workload>` and watch live timings
race against nDB.

**Why this exists.** Comparing in-process nDB to networked Postgres
flatters nDB on every workload that finishes faster than a libpq
round-trip (~15-20 μs floor). SQLite IS in-process — Python's stdlib
`sqlite3` module is FFI to the bundled C library, no socket, no
separate server process. Putting nDB, SQLite, and PG in the same
chart pulls apart "embedded vs networked" from "graph-shaped storage
vs relational storage". The columns where nDB ≈ SQLite ≫ PG are
honestly "the cost of being a server." The columns where nDB ≫
SQLite + PG are real architectural wins for nDB's data model.

Surface (matches the nDB and PG siblings byte-for-byte):
    GET  /health
    GET  /workloads
    POST /run/<name>
    POST /stress  (body: {"workload","concurrency","duration_ms"})
    GET  /stats

Database lifecycle:
    - File-backed at $BENCH_SQLITE_PATH (default
      `.demo-data/ndb-bench-race-sqlite.db`). On startup, reuse if
      the file is present + non-empty; else create + load + ANALYZE.
    - To force a fresh reload, set DROP_FIRST=1 in env.

Read-only: same as the nDB + PG sides — no `commits_per_sec` workload.

Run with:
    python3 tools/bench/race_sqlite_server.py --bind 127.0.0.1:8773
"""

from __future__ import annotations
import argparse
import json
import os
import sqlite3
import sys
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Lock, local

BENCH_DB_PATH = os.environ.get(
    "BENCH_SQLITE_PATH",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..",
                 ".demo-data", "ndb-bench-race-sqlite.db"),
)
BENCH_DB_PATH = os.path.normpath(BENCH_DB_PATH)
DROP_FIRST = os.environ.get("DROP_FIRST") == "1"

N_CUSTOMERS = 49_000
N_REGIONS = 1_000
N_SALES = 45_000
N_CONTAINS = 5_000

# Iter counts — must keep total wall-clock per click ≤ ~500 ms so the
# live race feels snappy. Mirrors `bench_race.rs` and the PG sibling.
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
     "blurb": "Look up customers by region code via the index, 500 times.",
     "iters": N_ITER_LOOKUPS},
    {"name": "single_pattern_query", "label": "Single-pattern query",
     "blurb": "SELECT id FROM customer WHERE region_code = 'REG-00000'",
     "iters": N_ITER_QUERY_SMALL},
    {"name": "two_pattern_join", "label": "Two-pattern join",
     "blurb": "Customer × sales junction (B-tree join)",
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


# ─── Connection helpers ──────────────────────────────────────────────

def _tune(conn: sqlite3.Connection) -> sqlite3.Connection:
    """Apply pragmas every connection should run with. Same values as
    SQLite's own "fast and safe" recipe — WAL for multi-reader
    concurrency, NORMAL fsync for ~10× write throughput at zero
    durability cost in practice, 64 MiB page cache, in-memory temp."""
    conn.execute("PRAGMA journal_mode = WAL")
    conn.execute("PRAGMA synchronous = NORMAL")
    conn.execute("PRAGMA cache_size = -65536")   # 64 MiB
    conn.execute("PRAGMA temp_store = MEMORY")
    conn.execute("PRAGMA mmap_size = 268435456") # 256 MiB
    return conn


def _connect(read_only: bool = False) -> sqlite3.Connection:
    """Open a connection to the bench DB. Read-only mode opens via the
    URI form so SQLite skips its own write-side bookkeeping. Always
    sets `check_same_thread=False` so workers can pass the connection
    around — we never share one connection between threads, but the
    Python wrapper insists on a flag."""
    if read_only:
        uri = f"file:{BENCH_DB_PATH}?mode=ro"
        conn = sqlite3.connect(uri, uri=True, check_same_thread=False)
    else:
        conn = sqlite3.connect(BENCH_DB_PATH, check_same_thread=False)
    return _tune(conn)


# ─── Database lifecycle ──────────────────────────────────────────────

def ensure_database() -> tuple[float, dict]:
    """Create or reuse the bench DB. Returns (load-ms, sample-data)."""
    os.makedirs(os.path.dirname(BENCH_DB_PATH), exist_ok=True)
    if DROP_FIRST and os.path.exists(BENCH_DB_PATH):
        for suffix in ("", "-wal", "-shm"):
            p = BENCH_DB_PATH + suffix
            if os.path.exists(p):
                os.remove(p)

    needs_load = not os.path.exists(BENCH_DB_PATH) or os.path.getsize(BENCH_DB_PATH) == 0
    t0 = time.perf_counter()
    if needs_load:
        log(f"creating {BENCH_DB_PATH}...")
        with _connect() as conn:
            init_schema(conn)
            samples = load_data(conn)
            conn.execute("ANALYZE")
            conn.commit()
        log(f"loaded {N_CUSTOMERS + N_REGIONS} entities + "
            f"{N_SALES + N_CONTAINS} junction rows in {(time.perf_counter() - t0) * 1000:.0f} ms")
    else:
        log(f"reusing existing {BENCH_DB_PATH}; sampling probes...")
        with _connect(read_only=True) as conn:
            samples = sample_existing(conn)
        log("samples ready")
    return (time.perf_counter() - t0) * 1000.0, samples


def init_schema(conn: sqlite3.Connection) -> None:
    """Same junction-table shape as the PG sibling — UUIDs stored as
    canonical lowercase TEXT (`str(uuid.uuid4())`). SQLite has no
    native UUID type but its TEXT comparison + B-tree behave fine."""
    conn.executescript("""
        CREATE TABLE region (
            id   TEXT PRIMARY KEY,
            code TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL
        );
        CREATE TABLE customer (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            region_code TEXT NOT NULL
        );
        CREATE INDEX customer_region_idx ON customer (region_code);
        CREATE TABLE sales (
            id       TEXT PRIMARY KEY,
            buyer_id TEXT NOT NULL REFERENCES customer(id)
        );
        CREATE INDEX sales_buyer_idx ON sales (buyer_id);
        CREATE TABLE contains (
            id        TEXT PRIMARY KEY,
            parent_id TEXT NOT NULL REFERENCES region(id),
            child_id  TEXT NOT NULL REFERENCES region(id)
        );
        CREATE INDEX contains_parent_idx ON contains (parent_id);
    """)


def load_data(conn: sqlite3.Connection) -> dict:
    region_codes: list[str] = []
    region_ids: list[str] = []
    rows = []
    for i in range(N_REGIONS):
        rid = str(uuid.uuid4())
        code = f"REG-{i:05d}"
        rows.append((rid, code, f"Region {i}"))
        region_codes.append(code)
        region_ids.append(rid)
    conn.executemany("INSERT INTO region (id, code, name) VALUES (?, ?, ?)", rows)

    cust_ids: list[str] = []
    batch = []
    for i in range(N_CUSTOMERS):
        cid = str(uuid.uuid4())
        cust_ids.append(cid)
        batch.append((cid, f"Customer {i}", region_codes[i % N_REGIONS]))
        if len(batch) >= 5000:
            conn.executemany("INSERT INTO customer (id, name, region_code) VALUES (?, ?, ?)", batch)
            batch = []
    if batch:
        conn.executemany("INSERT INTO customer (id, name, region_code) VALUES (?, ?, ?)", batch)

    batch = []
    for i in range(N_SALES):
        # Same buyer distribution as the PG sibling: 50% concentrated on
        # every-20th customer (sales "hub" pattern), 50% scattered.
        if i % 2 == 0:
            cust_idx = (((i // 2) % ((N_CUSTOMERS + 19) // 20)) * 20) % N_CUSTOMERS
        else:
            cust_idx = (i * 31 + 7) % N_CUSTOMERS
        batch.append((str(uuid.uuid4()), cust_ids[cust_idx]))
        if len(batch) >= 5000:
            conn.executemany("INSERT INTO sales (id, buyer_id) VALUES (?, ?)", batch)
            batch = []
    if batch:
        conn.executemany("INSERT INTO sales (id, buyer_id) VALUES (?, ?)", batch)

    n = N_REGIONS
    batch = []
    for i in range(N_CONTAINS):
        parent_idx = i % n
        child_idx = (parent_idx + n // 4 + (i // n) * (n // 8)) % n
        if parent_idx == child_idx:
            continue
        batch.append((str(uuid.uuid4()), region_ids[parent_idx], region_ids[child_idx]))
        if len(batch) >= 5000:
            conn.executemany("INSERT INTO contains (id, parent_id, child_id) VALUES (?, ?, ?)", batch)
            batch = []
    if batch:
        conn.executemany("INSERT INTO contains (id, parent_id, child_id) VALUES (?, ?, ?)", batch)

    conn.commit()
    return _materialize_samples(region_codes, region_ids, cust_ids)


def sample_existing(conn: sqlite3.Connection) -> dict:
    """Reuse: pull a deterministic sample of probes from the existing DB."""
    cur = conn.execute("SELECT code FROM region ORDER BY code LIMIT ?", (N_ITER_LOOKUPS,))
    codes = [r[0] for r in cur.fetchall()]
    cur = conn.execute("SELECT id FROM region ORDER BY code LIMIT 4")
    region_ids = [r[0] for r in cur.fetchall()]
    cur = conn.execute("SELECT id FROM customer ORDER BY id LIMIT ?", (N_ITER_LOOKUPS,))
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
        "min_us": float(samples_us[0]) if n else 0.0,
        "p50_us": float(samples_us[n // 2]) if n else 0.0,
        "p99_us": float(samples_us[min(n - 1, n * 99 // 100)]) if n else 0.0,
        "ops_per_sec": (n * 1_000_000.0 / total_us) if total_us > 0 else 0.0,
        "total_ms": total_us / 1000.0,
    }


def run_workload(conn: sqlite3.Connection, name: str, samples: dict) -> dict:
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


def _bench_iter_all(conn: sqlite3.Connection) -> dict:
    samples, outer = [], time.perf_counter()
    for _ in range(N_ITER_ITERATE):
        t = time.perf_counter()
        n = 0
        for _ in conn.execute(
            "SELECT id FROM region UNION ALL SELECT id FROM customer UNION ALL "
            "SELECT id FROM sales UNION ALL SELECT id FROM contains"
        ):
            n += 1
        samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("iter_all", samples, (time.perf_counter() - outer) * 1e6)


def _bench_point_lookup(conn: sqlite3.Connection, ids: list) -> dict:
    samples, outer = [], time.perf_counter()
    for cid in ids:
        t = time.perf_counter()
        conn.execute(
            "SELECT id, name, region_code FROM customer WHERE id = ?", (cid,)
        ).fetchone()
        samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("point_lookup", samples, (time.perf_counter() - outer) * 1e6)


def _bench_property_lookup(conn: sqlite3.Connection, codes: list[str]) -> dict:
    samples, outer = [], time.perf_counter()
    for code in codes:
        t = time.perf_counter()
        conn.execute(
            "SELECT id FROM customer WHERE region_code = ?", (code,)
        ).fetchall()
        samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("property_lookup", samples, (time.perf_counter() - outer) * 1e6)


def _bench_single_pattern(conn: sqlite3.Connection, region: str) -> dict:
    samples, outer = [], time.perf_counter()
    for _ in range(N_ITER_QUERY_SMALL):
        t = time.perf_counter()
        conn.execute(
            "SELECT id FROM customer WHERE region_code = ?", (region,)
        ).fetchall()
        samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("single_pattern_query", samples, (time.perf_counter() - outer) * 1e6)


def _bench_two_pattern_join(conn: sqlite3.Connection, region: str) -> dict:
    samples, outer = [], time.perf_counter()
    for _ in range(N_ITER_QUERY_LARGE):
        t = time.perf_counter()
        conn.execute("""
            SELECT c.id, s.id
              FROM customer c
              JOIN sales s ON s.buyer_id = c.id
             WHERE c.region_code = ?
        """, (region,)).fetchall()
        samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("two_pattern_join", samples, (time.perf_counter() - outer) * 1e6)


def _bench_recursive(conn: sqlite3.Connection, root) -> dict:
    samples, outer = [], time.perf_counter()
    for _ in range(N_ITER_RECURSIVE):
        t = time.perf_counter()
        conn.execute("""
            WITH RECURSIVE walk(node, depth) AS (
                SELECT child_id, 1 FROM contains WHERE parent_id = ?
                UNION
                SELECT c.child_id, w.depth + 1
                  FROM contains c JOIN walk w ON c.parent_id = w.node
                 WHERE w.depth < 3
            )
            SELECT DISTINCT node FROM walk
        """, (root,)).fetchall()
        samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("recursive_contains_depth3", samples, (time.perf_counter() - outer) * 1e6)


def _bench_count_aggregate(conn: sqlite3.Connection) -> dict:
    samples, outer = [], time.perf_counter()
    for _ in range(N_ITER_QUERY_LARGE):
        t = time.perf_counter()
        conn.execute("SELECT count(*) FROM customer").fetchone()
        samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("count_aggregate", samples, (time.perf_counter() - outer) * 1e6)


# ─── HTTP server ──────────────────────────────────────────────────────

class State:
    def __init__(self, samples: dict, load_ms: float):
        self.samples = samples
        self.load_ms = load_ms
        # Single shared connection for the serialised /run path. WAL
        # mode permits concurrent readers via the per-thread pool below
        # — only the controlled-race path goes through this connection.
        self.conn = _connect(read_only=True)
        self.conn_lock = Lock()
        self.rate_limit: dict[tuple[str, str], float] = {}
        self.rate_lock = Lock()
        # Per-thread read connection pool — sqlite3 connections are
        # cheap to open (a fopen + page-header read) and aren't
        # shareable across threads in practice, so we lazy-spawn one
        # per worker on first use.
        self._tl = local()

    def thread_conn(self) -> sqlite3.Connection:
        c = getattr(self._tl, "conn", None)
        if c is None:
            c = _connect(read_only=True)
            self._tl.conn = c
        return c


def make_handler(state: State):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):  # silence default access log
            return

        def _send_json(self, status: int, body):
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
                    "engine": f"sqlite {sqlite3.sqlite_version}-py",
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
            # Rate limit (per-IP, per-workload).
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
            with state.conn_lock:
                try:
                    out = run_workload(state.conn, name, state.samples)
                except Exception as e:
                    log(f"workload {name} failed: {e}")
                    try:
                        state.conn.close()
                    except Exception:
                        pass
                    state.conn = _connect(read_only=True)
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
    """Fire `concurrency` worker threads, each holding its own
    per-thread read-only connection (SQLite WAL mode permits N concurrent
    readers; the connections are cheap). Each worker loops the
    workload until deadline. Merge per-thread latencies, compute
    percentiles + a log10 histogram, return."""
    import threading
    deadline = time.monotonic() + (duration_ms / 1000.0)
    started = time.perf_counter()
    samples = state.samples

    latencies: list[list[int]] = [[] for _ in range(concurrency)]
    errors = [0] * concurrency

    def worker(tid: int) -> None:
        conn = _connect(read_only=True)
        idx_seed = (tid * 0x9e3779b97f4a7c15) & 0xFFFFFFFFFFFFFFFF
        try:
            while time.monotonic() < deadline:
                idx_seed ^= (idx_seed << 13) & 0xFFFFFFFFFFFFFFFF
                idx_seed ^= idx_seed >> 7
                idx_seed ^= (idx_seed << 17) & 0xFFFFFFFFFFFFFFFF
                idx = idx_seed
                t0 = time.perf_counter()
                try:
                    _do_one_op(conn, workload_name, samples, idx)
                    latencies[tid].append(int((time.perf_counter() - t0) * 1e6))
                except Exception:
                    errors[tid] += 1
        finally:
            try:
                conn.close()
            except Exception:
                pass

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
    """Snapshot disk + RAM + CPU. Disk = .db + -wal + -shm sidecars.
    SQLite is in-process — RSS + CPU come from this Python server's
    own /proc/self entries since there's no separate DB daemon."""
    bytes_on_disk = 0
    for suffix in ("", "-wal", "-shm"):
        p = BENCH_DB_PATH + suffix
        try:
            bytes_on_disk += os.path.getsize(p)
        except OSError:
            pass
    pid = os.getpid()
    rss_kb = _proc_rss_kb(pid)
    cpu_us = _proc_cpu_us(pid)
    return {
        "bytes_on_disk": bytes_on_disk,
        "bytes_resident": rss_kb * 1024,
        "cpu_user_us": cpu_us,
        "cpu_sys_us": 0,
        "backend_pids": 1,  # in-process — Python server IS the backend
    }


def _proc_rss_kb(pid: int) -> int:
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except (OSError, ValueError):
        pass
    return 0


def _proc_cpu_us(pid: int) -> int:
    """Sum (utime + stime) for one process, in microseconds (100 Hz
    clock — standard on every Linux distro)."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            raw = f.read()
        rest = raw.rsplit(") ", 1)[1]
        parts = rest.split()
        utime = int(parts[11])
        stime = int(parts[12])
        return (utime + stime) * 10_000
    except (OSError, ValueError, IndexError):
        return 0


def _do_one_op(conn: sqlite3.Connection, workload_name: str, samples: dict, idx: int) -> None:
    """One iteration of the named workload — fastest path possible."""
    if workload_name == "iter_all":
        for _ in conn.execute(
            "SELECT id FROM region UNION ALL SELECT id FROM customer UNION ALL "
            "SELECT id FROM sales UNION ALL SELECT id FROM contains"
        ):
            pass
    elif workload_name == "point_lookup":
        pool = samples["lookup_ids"]
        conn.execute(
            "SELECT id, name, region_code FROM customer WHERE id = ?",
            (pool[idx % len(pool)],),
        ).fetchone()
    elif workload_name == "property_lookup":
        pool = samples["lookup_codes"]
        conn.execute(
            "SELECT id FROM customer WHERE region_code = ?",
            (pool[idx % len(pool)],),
        ).fetchall()
    elif workload_name == "single_pattern_query":
        conn.execute(
            "SELECT id FROM customer WHERE region_code = ?",
            (samples["narrow_region"],),
        ).fetchall()
    elif workload_name == "two_pattern_join":
        conn.execute("""
            SELECT c.id, s.id
              FROM customer c
              JOIN sales s ON s.buyer_id = c.id
             WHERE c.region_code = ?
        """, (samples["narrow_region"],)).fetchall()
    elif workload_name == "recursive_contains_depth3":
        conn.execute("""
            WITH RECURSIVE walk(node, depth) AS (
                SELECT child_id, 1 FROM contains WHERE parent_id = ?
                UNION
                SELECT c.child_id, w.depth + 1
                  FROM contains c JOIN walk w ON c.parent_id = w.node
                 WHERE w.depth < 3
            )
            SELECT DISTINCT node FROM walk
        """, (samples["chain_root"],)).fetchall()
    elif workload_name == "count_aggregate":
        conn.execute("SELECT count(*) FROM customer").fetchone()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bind", default="127.0.0.1:8773")
    args = ap.parse_args()

    load_ms, samples = ensure_database()
    state = State(samples, load_ms)

    host, port = args.bind.rsplit(":", 1)
    server = ThreadingHTTPServer((host, int(port)), make_handler(state))
    log(f"bench-race SQLite serving on http://{host}:{port}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        log("shutdown")
    return 0


if __name__ == "__main__":
    sys.exit(main())
