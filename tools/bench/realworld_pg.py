#!/usr/bin/env python3
"""Postgres sibling of the realworld nDB micro-benchmark.

Loads the same shape (50_000 customers + 1_000 regions + 50_000 sales
junction rows + 5_000 contains rows) and measures eight matching
workloads. Output is a single JSON document on STDOUT, in the same
shape as `realworld_bench.rs`, so a downstream script can merge them
side-by-side.

Schema is intentionally idiomatic SQL (NOT cute):
- `customer(id uuid pk, name text, region_code text)`  — index on region_code
- `region(id uuid pk, code text unique, name text)`
- `sales(buyer_id uuid fk)` — pure 1-arity junction
- `contains(parent_id uuid fk, child_id uuid fk)` — closure walked via WITH RECURSIVE

Run with:
    DATABASE_URL=postgres://user:pass@host/db python3 tools/bench/realworld_pg.py
or
    python3 tools/bench/realworld_pg.py  # uses local socket, see DATABASE_URL_DEFAULT

The script creates a tmp database `ndb_realworld_bench_<pid>`, runs the
bench, then drops it. Failures leave the DB behind for inspection.
"""

from __future__ import annotations
import json
import os
import statistics
import subprocess
import sys
import time
import uuid
from contextlib import contextmanager

try:
    import psycopg
except ImportError:
    sys.exit("psycopg3 required: pip install --user --break-system-packages psycopg[binary]")

DATABASE_URL_DEFAULT = os.environ.get("DATABASE_URL", "postgresql:///postgres")
N_CUSTOMERS = 49_000
N_REGIONS = 1_000
N_SALES = 45_000
N_CONTAINS = 5_000
N_LOOKUPS = 1_000
N_COMMITS = 1_000


def log(msg: str) -> None:
    print(msg, file=sys.stderr)


@contextmanager
def tmp_db(base_url: str):
    """Create a temp DB on the same server as base_url, yield its URL, drop after."""
    dbname = f"ndb_realworld_bench_{os.getpid()}"
    with psycopg.connect(base_url, autocommit=True) as admin:
        admin.execute(f"DROP DATABASE IF EXISTS {dbname}")
        admin.execute(f"CREATE DATABASE {dbname}")
    if base_url.endswith("/postgres"):
        url = base_url[:-len("/postgres")] + f"/{dbname}"
    elif "://" in base_url and "/" not in base_url.split("://", 1)[1]:
        url = base_url + f"/{dbname}"
    else:
        # Best-effort path replace.
        url = base_url.rsplit("/", 1)[0] + f"/{dbname}"
    log(f"tmp DB: {url}")
    try:
        yield url
    finally:
        with psycopg.connect(base_url, autocommit=True) as admin:
            admin.execute(f"DROP DATABASE IF EXISTS {dbname}")


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


def load_data(conn: psycopg.Connection) -> tuple[list[str], list[uuid.UUID], list[uuid.UUID]]:
    log("loading regions...")
    region_codes: list[str] = []
    region_ids: list[uuid.UUID] = []
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

    log("loading customers...")
    cust_ids: list[uuid.UUID] = []
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

    log("loading sales...")
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

    log("loading contains...")
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
                cur.executemany(
                    "INSERT INTO contains (id, parent_id, child_id) VALUES (%s, %s, %s)", batch
                )
                batch = []
        if batch:
            cur.executemany("INSERT INTO contains (id, parent_id, child_id) VALUES (%s, %s, %s)", batch)
    conn.commit()
    log("analyse...")
    conn.execute("ANALYZE")
    conn.commit()

    return region_codes, region_ids, cust_ids


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
    }


def bench_iter_all(conn: psycopg.Connection) -> dict:
    samples, outer = [], time.perf_counter()
    for _ in range(5):
        t = time.perf_counter()
        n = 0
        with conn.cursor("iter") as cur:  # server-side cursor → streaming
            cur.itersize = 5000
            cur.execute(
                "SELECT id FROM region UNION ALL SELECT id FROM customer UNION ALL "
                "SELECT id FROM sales UNION ALL SELECT id FROM contains"
            )
            for _row in cur:
                n += 1
        assert n > 0
        samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("iter_all", samples, (time.perf_counter() - outer) * 1e6)


def bench_point_lookup(conn: psycopg.Connection, ids: list[uuid.UUID]) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for cid in ids:
            t = time.perf_counter()
            cur.execute("SELECT id, name, region_code FROM customer WHERE id = %s", (cid,))
            row = cur.fetchone()
            assert row is not None
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("point_lookup", samples, (time.perf_counter() - outer) * 1e6)


def bench_property_lookup(conn: psycopg.Connection, codes: list[str]) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for code in codes:
            t = time.perf_counter()
            cur.execute("SELECT id FROM customer WHERE region_code = %s", (code,))
            rows = cur.fetchall()
            assert len(rows) > 0
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("property_lookup", samples, (time.perf_counter() - outer) * 1e6)


def bench_single_pattern_query(conn: psycopg.Connection, region: str) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for _ in range(100):
            t = time.perf_counter()
            cur.execute("SELECT id FROM customer WHERE region_code = %s", (region,))
            rows = cur.fetchall()
            assert rows
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("single_pattern_query", samples, (time.perf_counter() - outer) * 1e6)


def bench_two_pattern_join(conn: psycopg.Connection, region: str) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for _ in range(100):
            t = time.perf_counter()
            cur.execute(
                """
                SELECT c.id, s.id
                  FROM customer c
                  JOIN sales s ON s.buyer_id = c.id
                 WHERE c.region_code = %s
                """,
                (region,),
            )
            rows = cur.fetchall()
            assert rows is not None
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("two_pattern_join", samples, (time.perf_counter() - outer) * 1e6)


def bench_recursive_contains(conn: psycopg.Connection, root: uuid.UUID) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for _ in range(50):
            t = time.perf_counter()
            cur.execute(
                """
                WITH RECURSIVE walk(node, depth) AS (
                    SELECT child_id, 1 FROM contains WHERE parent_id = %s
                    UNION
                    SELECT c.child_id, w.depth + 1
                      FROM contains c JOIN walk w ON c.parent_id = w.node
                     WHERE w.depth < 3
                )
                SELECT DISTINCT node FROM walk
                """,
                (root,),
            )
            cur.fetchall()
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("recursive_contains_depth3", samples, (time.perf_counter() - outer) * 1e6)


def bench_count_aggregate(conn: psycopg.Connection) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for _ in range(50):
            t = time.perf_counter()
            cur.execute("SELECT count(*) FROM customer")
            cur.fetchone()
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("count_aggregate", samples, (time.perf_counter() - outer) * 1e6)


def bench_commits_per_sec(conn: psycopg.Connection) -> dict:
    samples, outer = [], time.perf_counter()
    with conn.cursor() as cur:
        for i in range(N_COMMITS):
            t = time.perf_counter()
            cur.execute(
                "INSERT INTO customer (id, name, region_code) VALUES (%s, %s, %s)",
                (uuid.uuid4(), f"bench-{i}", "REG-00000"),
            )
            conn.commit()
            samples.append(int((time.perf_counter() - t) * 1e6))
    return finalize("commits_per_sec", samples, (time.perf_counter() - outer) * 1e6)


def db_size_bytes(conn: psycopg.Connection) -> int:
    cur = conn.execute("SELECT pg_database_size(current_database())")
    row = cur.fetchone()
    return int(row[0]) if row else 0


def resident_kb() -> int:
    try:
        for line in open("/proc/self/status"):
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    except OSError:
        pass
    return 0


def main() -> int:
    import random
    with tmp_db(DATABASE_URL_DEFAULT) as url:
        with psycopg.connect(url) as conn:
            init_schema(conn)
            t0 = time.perf_counter()
            codes, region_ids, cust_ids = load_data(conn)
            load_ms = (time.perf_counter() - t0) * 1000.0
            log(f"loaded {N_CUSTOMERS + N_REGIONS} entities + "
                f"{N_SALES + N_CONTAINS} junction rows in {load_ms:.0f} ms")

            rng = random.Random(0xdeadbeef)
            lookup_ids = rng.choices(cust_ids, k=N_LOOKUPS)
            lookup_codes = rng.choices(codes, k=N_LOOKUPS)
            narrow_region = codes[0]
            root = region_ids[0]

            results = [
                bench_iter_all(conn),
                bench_point_lookup(conn, lookup_ids),
                bench_property_lookup(conn, lookup_codes),
                bench_single_pattern_query(conn, narrow_region),
                bench_two_pattern_join(conn, narrow_region),
                bench_recursive_contains(conn, root),
                bench_count_aggregate(conn),
                bench_commits_per_sec(conn),
            ]
            bytes_on_disk = db_size_bytes(conn)

    bytes_resident = resident_kb() * 1024
    out = {
        "engine": f"postgres {sys.version.split()[0]}-py",
        "workload": "realworld_microbench",
        "n_entities": N_CUSTOMERS + N_REGIONS,
        "n_hyperedges": N_SALES + N_CONTAINS,
        "load_ms": load_ms,
        "flush_ms": 0.0,
        "bytes_on_disk": bytes_on_disk,
        "bytes_resident": bytes_resident,
        "results": results,
    }
    log("")
    log("| workload | iters | min μs | p50 μs | p99 μs | thr ops/s |")
    log("|---|---:|---:|---:|---:|---:|")
    for r in results:
        log(f"| {r['name']} | {r['iters']} | "
            f"{r['min_us']:.0f} | {r['p50_us']:.0f} | {r['p99_us']:.0f} | "
            f"{r['ops_per_sec']:.0f} |")
    log("")
    log(f"bytes_on_disk:  {bytes_on_disk:>12} ({bytes_on_disk / 1024 / 1024:.1f} MiB)")
    log(f"bytes_resident: {bytes_resident:>12} ({bytes_resident / 1024 / 1024:.1f} MiB)")

    json.dump(out, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
