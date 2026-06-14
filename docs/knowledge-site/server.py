#!/usr/bin/env python3
"""
nDB knowledge-site server.

Three jobs in one process:

  1. Serve the static knowledge site from this directory.
  2. Reverse-proxy the alphafold_ndb live demo (two upstream ports)
     under /alphafold_ndb/.
  3. Power a feedback widget backed by a dedicated nDB instance
     (a second `ndb-server` process on :8744 with its own DB).
     POSTs at /api/feedback → entity in nDB → notify-send + log.

  /alphafold_ndb/api/*  → http://127.0.0.1:8742/*   (demo ndb-server API)
  /alphafold_ndb/...    → http://127.0.0.1:9876/... (demo static + index.html)
  /api/feedback         → write feedback entity to http://127.0.0.1:8744
  /api/feedback/list    → list all feedback entities (admin)
  /api/feedback/<id>/status → update status (admin)
  everything else       → static files in this directory, with the feedback
                          widget JS + CSS injected before </body>

stdlib only. Run:
    python3 server.py
Listens on 127.0.0.1:9880.
"""
from __future__ import annotations

import html
import http.server
import socketserver
import urllib.request
import urllib.error
import urllib.parse
import json
import os
import re
import shutil
import subprocess
import sys
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

# Optional Telegram push — reuse the existing ANSD wrapper which reads
# the bot token from ~/.claude/channels/telegram/.env and the chat_id
# from access.json. If the wrapper or config is missing, telegram_push()
# returns False silently so the server keeps working.
_TG_SKILL_PATH = "/home/long/.claude/skills/ansd-shared"
try:
    if _TG_SKILL_PATH not in sys.path:
        sys.path.insert(0, _TG_SKILL_PATH)
    from telegram_push import push as _tg_push, is_configured as _tg_configured  # type: ignore
except Exception:  # noqa: BLE001
    def _tg_push(text: str, parse_mode: str = "HTML", chat_id=None) -> bool:  # type: ignore
        return False
    def _tg_configured() -> bool:  # type: ignore
        return False

LISTEN_HOST = "127.0.0.1"
LISTEN_PORT = 9880
# App-layer langgraph view server (bounded /view/* tiles). Optional — the
# explorer falls back to the static graph.json if it's not running.
LANGGRAPH_API = os.environ.get("LANGGRAPH_API", "http://127.0.0.1:8791")

SITE_ROOT = Path(__file__).resolve().parent

# Each demo gets a (static-port, api-port, url-prefix) triple. The Python
# dispatcher iterates DEMOS to route requests. Adding a new demo is a
# single dict entry here + a launcher entry + a narrative page.
# Ports here match the deployed read-only API servers (ndb-srv-*) and static
# servers (ndb-web-*) on the VPS. Three demos are live.
DEMOS = [
    {"prefix": "/alphafold_ndb",  "static": "http://127.0.0.1:9101", "api": "http://127.0.0.1:8743",
     # docs/explorer hardcodes API_URL="http://127.0.0.1:8742"; rewrite it to
     # same-origin so the browser stays on this host.
     "html_api_rewrite_from": b'"http://127.0.0.1:8742"',
     "html_api_rewrite_to":   b'(window.location.origin + "/alphafold_ndb/api")'},
    {"prefix": "/exoplanet_ndb",  "static": "http://127.0.0.1:9102", "api": "http://127.0.0.1:8746",
     # exoplanet SPA derives same-origin from location.pathname — no rewrite.
     "html_api_rewrite_from": b'',
     "html_api_rewrite_to":   b''},
    {"prefix": "/biodiv_ndb",     "static": "http://127.0.0.1:9103", "api": "http://127.0.0.1:8749",
     "html_api_rewrite_from": b'',
     "html_api_rewrite_to":   b''},
]

# Studio (read-only GUI) mounted under /studio. Its frontend calls
# fetch("/api"+path) — rewritten to "/studio/api" on the served HTML so all
# calls stay under the prefix. The machine wire API is proxied at /v1.
STUDIO_UPSTREAM = "http://127.0.0.1:8780"
TRIO_V1_UPSTREAM = "http://127.0.0.1:8742"

# Back-compat for the old single-demo constants (unused now but kept so
# diffs against earlier server.py read cleanly).
DEMO_STATIC = DEMOS[0]["static"]
DEMO_API = DEMOS[0]["api"]
DEMO_PREFIX = DEMOS[0]["prefix"]

# Upstream for the feedback nDB. Separate engine + DB from the demo.
FEEDBACK_API = "http://127.0.0.1:8744"

# Live bench-race backends. Each one preloads the same 100k realworld
# shape and exposes /health, /workloads, /run/<name>, /stress, /stats.
# Reverse-proxied at /bench/<backend>/* so the static `bench.html` page
# can call them same-origin. SQLite is the embedded reference: comparing
# in-process nDB to in-process SQLite isolates storage-engine quality
# from the embedded-vs-networked architectural advantage that nDB has
# over PG.
BENCH_BACKENDS = {
    "/bench/ndb":         "http://127.0.0.1:8771",
    "/bench/pg":          "http://127.0.0.1:8772",
    "/bench/sqlite":      "http://127.0.0.1:8773",
    "/bench/sqlite-rust": "http://127.0.0.1:8774",
}

# Feedback schema. Type 200 for the feedback entity; property IDs are
# also in the 200s so they don't collide with the demo's biology schema
# (which uses 30-69) if anyone ever points the same client at both.
FB_TYPE_FEEDBACK = 200
FB_PROP_NAME = 200
FB_PROP_EMAIL = 201
FB_PROP_MESSAGE = 202
FB_PROP_TS = 203
FB_PROP_USER_AGENT = 204
FB_PROP_PAGE = 205
FB_PROP_STATUS = 206

# Race-result schema (lives in the same feedback nDB at port 8744 —
# this is a small enough volume of records to share the engine, and
# co-locating means one persistent storage + one backup story). Type
# 300 + props 300-310 are reserved for race results; no overlap with
# the feedback type at 200.
RR_TYPE_RACE_RESULT = 300
RR_PROP_WORKLOAD     = 300  # string
RR_PROP_BACKEND      = 301  # string: "ndb" | "pg" | "sqlite"
RR_PROP_MODE         = 302  # string: "controlled" | "stress"
RR_PROP_CONCURRENCY  = 303  # i64 (1 for controlled mode)
RR_PROP_DURATION_MS  = 304  # i64
RR_PROP_RPS          = 305  # f64
RR_PROP_P50_US       = 306  # f64
RR_PROP_P99_US       = 307  # f64
RR_PROP_TOTAL_OPS    = 308  # i64
RR_PROP_TS_MS        = 309  # i64 — milliseconds since Unix epoch
RR_PROP_RACE_ID      = 310  # string — UUID tying both sides of one race
RR_PROP_WINNER       = 311  # string: "ndb" | "pg" | "sqlite" — winner
RR_PROP_CHALLENGER   = 312  # string: "pg" | "sqlite" | "sqlite-rust" — what nDB raced
                            # against (records pre-dating this field default to "pg")
VALID_CHALLENGERS = ("pg", "sqlite", "sqlite-rust")

# Cap on how many race results we keep before pruning old ones. Pure
# defensive — the nDB engine handles much more, but the aggregates
# pass loads everything into memory for grouping.
RR_MAX_RECORDS = 20_000

# HTML-response rewrites.
# (1) Demo: hardcoded API host → same-origin path so the browser stays here.
DEMO_REWRITE_FROM = b'"http://127.0.0.1:8742"'
DEMO_REWRITE_TO = b'(window.location.origin + "/alphafold_ndb/api")'
# (2) Knowledge site: inject the feedback widget before </body>.
WIDGET_SNIPPET = (
    b'<link rel="stylesheet" href="/static/feedback-widget.css">'
    b'<script defer src="/static/feedback-widget.js"></script>'
)

# Local event log — appended on every feedback submit so the user has a
# durable record even if notify-send fails (no DISPLAY, X session dropped).
FEEDBACK_EVENT_LOG = "/tmp/ndb-feedback-events.log"


# ── utility ─────────────────────────────────────────────────────────────

def _utc_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _new_feedback_id():
    """UUIDv4 is fine here — feedback volume is low; total order isn't required."""
    return str(uuid.uuid4())


def _notify_desktop(title: str, body: str):
    """Best-effort desktop notification. No exception if it fails — server
    must keep running even with no $DISPLAY (e.g. when launched headless)."""
    if not shutil.which("notify-send"):
        return
    try:
        subprocess.Popen(
            [
                "notify-send",
                "--urgency=normal",
                "--icon=/home/long/.local/share/icons/ndb-services/ndb-services.svg",
                title,
                body,
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        pass


def _append_event_log(line: str):
    try:
        with open(FEEDBACK_EVENT_LOG, "a", encoding="utf-8") as f:
            f.write(line.rstrip("\n") + "\n")
    except Exception:
        pass


def _notify_telegram(fid: str, name: str, email: str, page: str, message: str):
    """Push a formatted message to the user's Telegram via the ANSD wrapper.
    Silent-fail if Telegram isn't configured — the desktop toast + event log
    remain as the durable record."""
    if not _tg_configured():
        return
    who = name or (email if email else "anonymous")
    # parse_mode=HTML so we get bold/code formatting; html.escape() guards
    # against any < > & in user input being interpreted as markup.
    text = (
        f"💬 <b>New nDB feedback</b>\n"
        f"From: <b>{html.escape(who)}</b>"
        + (f" &lt;{html.escape(email)}&gt;" if email else "")
        + (f"\nPage: <code>{html.escape(page)}</code>" if page else "")
        + f"\n\n{html.escape(message)}\n\n"
        f"<i>id {html.escape(fid[:8])}… · "
        f'<a href="https://ndb.nextstar-erp.com/feedback.html">open inbox</a></i>'
    )
    try:
        _tg_push(text, parse_mode="HTML")
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(f"[telegram] unexpected error: {exc}\n")


def _ndb_post(path: str, body) -> tuple[int, bytes]:
    """POST JSON to the feedback ndb-server. Returns (status, body_bytes)."""
    data = json.dumps(body).encode("utf-8")
    req = urllib.request.Request(
        FEEDBACK_API + path,
        data=data,
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, (e.read() if hasattr(e, "read") else b"")


def _ndb_get(path: str) -> tuple[int, bytes]:
    """GET JSON/JSONL from the feedback ndb-server. Returns (status, body_bytes)."""
    try:
        with urllib.request.urlopen(FEEDBACK_API + path, timeout=10) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, (e.read() if hasattr(e, "read") else b"")


def _read_all_feedback() -> list[dict]:
    """Stream /iter, keep only TYPE_FEEDBACK entities, return the latest
    asserted version of each entity_id. The engine is append-only —
    superseded versions show up too — so we dedupe by entity_id keeping
    the highest tx_id_assert."""
    status, payload = _ndb_get("/iter")
    if status != 200:
        return []
    latest: dict[str, dict] = {}
    for line in payload.splitlines():
        if not line.strip():
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("kind") != "entity":
            continue
        if rec.get("type_id") != FB_TYPE_FEEDBACK:
            continue
        if rec.get("tx_id_supersede") != "active":
            continue
        eid = rec.get("entity_id")
        if not eid:
            continue
        prev = latest.get(eid)
        if prev is None or rec.get("tx_id_assert", 0) > prev.get("tx_id_assert", 0):
            latest[eid] = rec
    return list(latest.values())


def _props_to_dict(rec: dict) -> dict:
    """Flatten the [{prop_id, value:{tag,value}}, ...] array into a flat
    dict keyed by prop_id. Saves the admin page from parsing the wire
    format itself."""
    out = {}
    for p in rec.get("properties", []):
        pid = p.get("prop_id")
        v = p.get("value", {}).get("value")
        out[pid] = v
    return out


# ── request handler ────────────────────────────────────────────────────

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(SITE_ROOT), **kwargs)

    # ── routing ─────────────────────────────────────────────────────
    def do_GET(self):
        self._dispatch("GET")

    def do_HEAD(self):
        self._dispatch("HEAD")

    def do_POST(self):
        self._dispatch("POST")

    def do_PUT(self):
        self._dispatch("PUT")

    def do_DELETE(self):
        self._dispatch("DELETE")

    def _dispatch(self, method: str):
        path = self.path
        bare, _, _ = path.partition("?")

        # ── feedback API (handled in-process — no proxy of /api to a
        #    raw nDB; the client never sees the engine schema directly). ──
        if bare == "/api/feedback":
            if method == "POST":
                self._feedback_submit()
                return
            if method == "GET":
                self._feedback_list()
                return
            self.send_response(405)
            self.end_headers()
            return

        if bare == "/api/feedback/list":
            self._feedback_list()
            return

        m = re.match(r"^/api/feedback/([0-9a-fA-F-]{8,40})/status$", bare)
        if m and method == "POST":
            self._feedback_status(m.group(1))
            return

        # ── race results: log + aggregate ──
        if bare == "/api/race/log" and method == "POST":
            self._race_log()
            return
        if bare == "/api/race/aggregates" and method == "GET":
            self._race_aggregates()
            return

        # ── bench-race proxy (live nDB vs PG) ──
        for pre, upstream in BENCH_BACKENDS.items():
            if bare == pre or bare.startswith(pre + "/"):
                self._proxy_bench(upstream, pre, method, path)
                return

        # ── langgraph_ndb live tiles: proxy /langgraph_ndb/api/* to the
        #    app-layer langgraph-server (bounded /view/* queries). The
        #    explorer uses these when reachable, else falls back to the
        #    committed static graph.json. ──
        if bare == "/langgraph_ndb/api" or bare.startswith("/langgraph_ndb/api/"):
            self._proxy_langgraph_api(self.path[len("/langgraph_ndb/api"):] or "/", method)
            return

        # ── langgraph_ndb: static explorer + committed CC0 graph.json ──
        if bare == "/langgraph_ndb":
            self.send_response(301)
            self.send_header("Location", "/langgraph_ndb/")
            self.end_headers()
            return
        if bare.startswith("/langgraph_ndb/") and method == "GET":
            self._serve_langgraph(bare[len("/langgraph_ndb/"):])
            return

        # ── machine wire API: /v1/* → trio read-only server (keeps /v1) ──
        if bare == "/v1" or bare.startswith("/v1/"):
            self._proxy_passthrough(TRIO_V1_UPSTREAM, path, method)
            return

        # ── Studio GUI under /studio (HTML /api refs rewritten to /studio/api) ──
        if bare == "/studio" or bare == "/studio/":
            self._proxy_studio("/", method)
            return
        if bare.startswith("/studio/"):
            self._proxy_studio(path[len("/studio"):], method)
            return

        # ── demo proxy (match the longest demo prefix) ──
        for demo in DEMOS:
            pre = demo["prefix"]
            if bare == pre or bare.startswith(pre + "/"):
                self._proxy_demo(demo, method, path)
                return

        # ── static (with widget injection on text/html) ──
        if method == "GET":
            return self._serve_static_with_widget()
        if method == "HEAD":
            return super().do_HEAD()

        self.send_response(405)
        self.end_headers()

    # ── feedback handlers ───────────────────────────────────────────
    def _feedback_submit(self):
        try:
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            payload = json.loads(raw or b"{}")
        except (ValueError, json.JSONDecodeError):
            return self._json(400, {"ok": False, "error": "bad_json"})

        message = (payload.get("message") or "").strip()
        if not message:
            return self._json(400, {"ok": False, "error": "message_required"})
        if len(message) > 4000:
            message = message[:4000]

        name = (payload.get("name") or "").strip()[:120]
        email = (payload.get("email") or "").strip()[:200]
        page = (payload.get("page") or "").strip()[:300]
        user_agent = self.headers.get("User-Agent", "")[:300]
        ts = _utc_iso()
        fid = _new_feedback_id()

        props = [
            {"prop_id": FB_PROP_MESSAGE,    "value": {"tag": "string", "value": message}},
            {"prop_id": FB_PROP_TS,         "value": {"tag": "string", "value": ts}},
            {"prop_id": FB_PROP_STATUS,     "value": {"tag": "string", "value": "new"}},
            {"prop_id": FB_PROP_USER_AGENT, "value": {"tag": "string", "value": user_agent}},
        ]
        if name:
            props.append({"prop_id": FB_PROP_NAME,  "value": {"tag": "string", "value": name}})
        if email:
            props.append({"prop_id": FB_PROP_EMAIL, "value": {"tag": "string", "value": email}})
        if page:
            props.append({"prop_id": FB_PROP_PAGE,  "value": {"tag": "string", "value": page}})

        record = {
            "kind": "entity",
            "entity_id": fid,
            "type_id": FB_TYPE_FEEDBACK,
            "tx_id_assert": 0,
            "tx_id_supersede": "active",
            "properties": props,
        }
        status, body = _ndb_post("/commit", {"records": [record]})
        if status >= 300:
            sys.stderr.write(f"feedback commit failed {status}: {body!r}\n")
            return self._json(502, {"ok": False, "error": "ndb_commit_failed"})

        # Notify + log AFTER the commit so we don't ping the user for a
        # write that didn't actually land. Three independent channels:
        #   1. desktop toast — instant if user is at the machine
        #   2. Telegram      — reaches phone, survives session
        #   3. event log     — durable record on disk
        preview = (message[:80] + "…") if len(message) > 80 else message
        who = name or (email if email else "anonymous")
        _notify_desktop("New nDB feedback", f"{who}: {preview}")
        _notify_telegram(fid, name, email, page, message)
        _append_event_log(json.dumps({
            "ts": ts, "id": fid, "name": name, "email": email,
            "page": page, "message": message,
        }, ensure_ascii=False))

        return self._json(200, {"ok": True, "id": fid})

    def _feedback_list(self):
        items = []
        for rec in _read_all_feedback():
            p = _props_to_dict(rec)
            items.append({
                "id":         rec.get("entity_id"),
                "ts":         p.get(FB_PROP_TS),
                "name":       p.get(FB_PROP_NAME, ""),
                "email":      p.get(FB_PROP_EMAIL, ""),
                "page":       p.get(FB_PROP_PAGE, ""),
                "message":    p.get(FB_PROP_MESSAGE, ""),
                "status":     p.get(FB_PROP_STATUS, "new"),
                "user_agent": p.get(FB_PROP_USER_AGENT, ""),
            })
        items.sort(key=lambda it: it.get("ts") or "", reverse=True)
        return self._json(200, {"ok": True, "items": items, "count": len(items)})

    def _feedback_status(self, fid: str):
        try:
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            payload = json.loads(raw or b"{}")
        except (ValueError, json.JSONDecodeError):
            return self._json(400, {"ok": False, "error": "bad_json"})

        new_status = (payload.get("status") or "").strip()
        if new_status not in ("new", "read", "resolved", "wontfix"):
            return self._json(400, {"ok": False, "error": "bad_status"})

        # Find the live record so we can rewrite-with-new-status. Engine
        # is append-only so we re-assert the entity at the same id with
        # the new status property; the prior version supersedes itself.
        for rec in _read_all_feedback():
            if rec.get("entity_id") != fid:
                continue
            props = []
            for p in rec.get("properties", []):
                pid = p.get("prop_id")
                if pid == FB_PROP_STATUS:
                    continue  # drop the old status; we'll re-emit it below
                props.append(p)
            props.append({
                "prop_id": FB_PROP_STATUS,
                "value": {"tag": "string", "value": new_status},
            })
            update = {
                "kind": "entity",
                "entity_id": fid,
                "type_id": FB_TYPE_FEEDBACK,
                "tx_id_assert": 0,
                "tx_id_supersede": "active",
                "properties": props,
            }
            status, body = _ndb_post("/commit", {"records": [update]})
            if status >= 300:
                return self._json(502, {"ok": False, "error": "ndb_commit_failed"})
            return self._json(200, {"ok": True, "id": fid, "status": new_status})

        return self._json(404, {"ok": False, "error": "not_found"})

    # ── race-result log + aggregate handlers ────────────────────────
    def _race_log(self):
        """Persist one completed race as 2 entity records in the
        feedback nDB (one per side). Body shape:
            {race_id, workload, mode, concurrency, duration_ms,
             ndb: {rps, p50_us, p99_us, total_ops},
             pg:  {rps, p50_us, p99_us, total_ops}}
        """
        try:
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            payload = json.loads(raw or b"{}")
        except (ValueError, json.JSONDecodeError):
            return self._json(400, {"ok": False, "error": "bad_json"})

        race_id = str(payload.get("race_id") or uuid.uuid4())
        workload = str(payload.get("workload") or "")[:80]
        mode = str(payload.get("mode") or "")
        if mode not in ("controlled", "stress"):
            return self._json(400, {"ok": False, "error": "bad_mode"})
        concurrency = int(payload.get("concurrency") or 1)
        duration_ms = int(payload.get("duration_ms") or 0)
        ndb = payload.get("ndb") or {}
        # Detect the challenger from whichever non-nDB side carries an
        # `rps` field. Backward-compatible: existing pages POST {ndb,pg};
        # the new SQLite mode POSTs {ndb,sqlite}. Future challengers
        # plug in by adding to VALID_CHALLENGERS.
        challenger = None
        for cand in VALID_CHALLENGERS:
            c = payload.get(cand)
            if isinstance(c, dict) and c.get("rps") is not None:
                challenger = cand
                break
        if challenger is None or not (
            isinstance(ndb, dict) and ndb.get("rps") is not None
        ):
            return self._json(400, {"ok": False, "error": "missing_sides"})
        other = payload[challenger]

        ndb_rps = float(ndb.get("rps") or 0)
        other_rps = float(other.get("rps") or 0)
        winner = "ndb" if ndb_rps >= other_rps else challenger
        ts_ms = int(time.time() * 1000)

        def _side_record(backend: str, m: dict) -> dict:
            return {
                "kind": "entity",
                "entity_id": str(uuid.uuid4()),
                "type_id": RR_TYPE_RACE_RESULT,
                "tx_id_assert": 0,
                "tx_id_supersede": "active",
                "properties": [
                    {"prop_id": RR_PROP_WORKLOAD,    "value": {"tag": "string", "value": workload}},
                    {"prop_id": RR_PROP_BACKEND,     "value": {"tag": "string", "value": backend}},
                    {"prop_id": RR_PROP_MODE,        "value": {"tag": "string", "value": mode}},
                    {"prop_id": RR_PROP_CONCURRENCY, "value": {"tag": "i64", "value": concurrency}},
                    {"prop_id": RR_PROP_DURATION_MS, "value": {"tag": "i64", "value": duration_ms}},
                    {"prop_id": RR_PROP_RPS,         "value": {"tag": "f64", "value": float(m.get("rps") or 0)}},
                    {"prop_id": RR_PROP_P50_US,      "value": {"tag": "f64", "value": float(m.get("p50_us") or 0)}},
                    {"prop_id": RR_PROP_P99_US,      "value": {"tag": "f64", "value": float(m.get("p99_us") or 0)}},
                    {"prop_id": RR_PROP_TOTAL_OPS,   "value": {"tag": "i64", "value": int(m.get("total_ops") or 0)}},
                    {"prop_id": RR_PROP_TS_MS,       "value": {"tag": "i64", "value": ts_ms}},
                    {"prop_id": RR_PROP_RACE_ID,     "value": {"tag": "string", "value": race_id}},
                    {"prop_id": RR_PROP_WINNER,      "value": {"tag": "string", "value": winner}},
                    {"prop_id": RR_PROP_CHALLENGER,  "value": {"tag": "string", "value": challenger}},
                ],
            }

        records = [_side_record("ndb", ndb), _side_record(challenger, other)]
        status, body = _ndb_post("/commit", {"records": records})
        if status >= 300:
            sys.stderr.write(f"race log commit failed {status}: {body!r}\n")
            return self._json(502, {"ok": False, "error": "ndb_commit_failed"})
        return self._json(200, {"ok": True, "race_id": race_id, "winner": winner})

    def _race_aggregates(self):
        """Stream all RR records, group by (workload, mode, concurrency,
        challenger), compute count + mean RPS + mean p50 + win counts.

        Query params:
        - `challenger=pg` (default) or `challenger=sqlite` — restrict
          output to one challenger's races. Returned rows still carry
          a `challenger` field so the frontend can label columns.
        """
        # Parse ?challenger=... from the path's query string.
        from urllib.parse import urlparse, parse_qs
        q = parse_qs(urlparse(self.path).query)
        want_challenger = (q.get("challenger") or ["pg"])[0]
        if want_challenger not in VALID_CHALLENGERS:
            want_challenger = "pg"

        all_rr = self._read_all_race_results()
        # Group by (workload, mode, concurrency, challenger). Records
        # pre-dating the challenger field default to "pg".
        groups: dict[tuple[str, str, int, str], dict] = {}
        for rec in all_rr:
            challenger = rec.get("challenger") or "pg"
            if challenger != want_challenger:
                continue
            key = (rec["workload"], rec["mode"], rec["concurrency"], challenger)
            g = groups.setdefault(key, {
                "workload": rec["workload"], "mode": rec["mode"],
                "concurrency": rec["concurrency"],
                "challenger": challenger,
                "ndb_rps_sum": 0.0, "ndb_p50_sum": 0.0, "ndb_count": 0,
                "chal_rps_sum": 0.0, "chal_p50_sum": 0.0, "chal_count": 0,
                "ndb_wins": 0, "chal_wins": 0, "race_ids": set(),
            })
            if rec["backend"] == "ndb":
                g["ndb_rps_sum"] += rec["rps"]
                g["ndb_p50_sum"] += rec["p50_us"]
                g["ndb_count"]   += 1
            else:
                g["chal_rps_sum"] += rec["rps"]
                g["chal_p50_sum"] += rec["p50_us"]
                g["chal_count"]   += 1
            # Count one win per race_id (per side it shows up twice).
            if rec["race_id"] not in g["race_ids"]:
                g["race_ids"].add(rec["race_id"])
                if rec["winner"] == "ndb": g["ndb_wins"]  += 1
                else:                      g["chal_wins"] += 1

        rows = []
        for g in groups.values():
            races = len(g["race_ids"])
            rows.append({
                "workload": g["workload"], "mode": g["mode"],
                "concurrency": g["concurrency"], "challenger": g["challenger"],
                "races": races,
                "ndb_avg_rps":  (g["ndb_rps_sum"]  / g["ndb_count"])  if g["ndb_count"]  else 0.0,
                "chal_avg_rps": (g["chal_rps_sum"] / g["chal_count"]) if g["chal_count"] else 0.0,
                "ndb_avg_p50_us":  (g["ndb_p50_sum"]  / g["ndb_count"])  if g["ndb_count"]  else 0.0,
                "chal_avg_p50_us": (g["chal_p50_sum"] / g["chal_count"]) if g["chal_count"] else 0.0,
                "ndb_wins": g["ndb_wins"], "chal_wins": g["chal_wins"],
            })
        rows.sort(key=lambda r: (r["mode"], r["workload"], r["concurrency"]))
        self._json(200, {"rows": rows, "challenger": want_challenger,
                         "total_records": len(all_rr)})

    def _read_all_race_results(self) -> list[dict]:
        status, payload = _ndb_get("/iter")
        if status != 200:
            return []
        out = []
        for line in payload.splitlines():
            if not line.strip(): continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            if rec.get("kind") != "entity": continue
            if rec.get("type_id") != RR_TYPE_RACE_RESULT: continue
            if rec.get("tx_id_supersede") != "active": continue
            props = {p.get("prop_id"): p.get("value", {}).get("value")
                     for p in rec.get("properties", [])}
            try:
                # Pre-v3-final records have no challenger field — default
                # to "pg" since that was the only challenger before SQLite
                # joined.
                challenger = str(props.get(RR_PROP_CHALLENGER) or "pg")
                out.append({
                    "workload":     str(props.get(RR_PROP_WORKLOAD) or ""),
                    "backend":      str(props.get(RR_PROP_BACKEND) or ""),
                    "challenger":   challenger,
                    "mode":         str(props.get(RR_PROP_MODE) or ""),
                    "concurrency":  int(props.get(RR_PROP_CONCURRENCY) or 0),
                    "rps":          float(props.get(RR_PROP_RPS) or 0),
                    "p50_us":       float(props.get(RR_PROP_P50_US) or 0),
                    "p99_us":       float(props.get(RR_PROP_P99_US) or 0),
                    "race_id":      str(props.get(RR_PROP_RACE_ID) or ""),
                    "winner":       str(props.get(RR_PROP_WINNER) or ""),
                })
            except (TypeError, ValueError):
                continue
        return out

    def _json(self, status: int, obj):
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # ── langgraph_ndb live-tile proxy → app-layer langgraph-server ──
    def _proxy_langgraph_api(self, sub: str, method: str):
        import urllib.request
        url = LANGGRAPH_API + sub
        body = None
        if method == "POST":
            n = int(self.headers.get("Content-Length", 0) or 0)
            body = self.rfile.read(n) if n else None
        try:
            req = urllib.request.Request(url, data=body, method=method)
            with urllib.request.urlopen(req, timeout=15) as r:
                data = r.read()
                ctype = r.headers.get("Content-Type", "application/json")
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except Exception as e:  # noqa: BLE001 — surface upstream-down to the client
            msg = json.dumps({"error": "langgraph-server unreachable", "detail": str(e)}).encode()
            self.send_response(502)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(msg)))
            self.end_headers()
            self.wfile.write(msg)

    # ── langgraph_ndb static files (docs/langgraph, sibling of SITE_ROOT) ──
    def _serve_langgraph(self, rel: str):
        import mimetypes
        root = (SITE_ROOT.parent / "langgraph").resolve()
        rel = rel or "index.html"
        if rel.endswith("/"):
            rel += "index.html"
        target = (root / rel).resolve()
        if not str(target).startswith(str(root) + os.sep) and target != root:
            self.send_response(403); self.end_headers(); return
        if not target.is_file():
            self.send_response(404); self.end_headers(); return
        data = target.read_bytes()
        ctype = mimetypes.guess_type(str(target))[0] or "application/octet-stream"
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    # ── static + widget injection ───────────────────────────────────
    def _serve_static_with_widget(self):
        """Hand off to SimpleHTTPRequestHandler logic but rewrite the
        body for text/html responses to inject the feedback widget."""
        # Resolve path the same way SimpleHTTPRequestHandler.translate_path
        # would — relative to SITE_ROOT.
        path = self.translate_path(self.path)
        if os.path.isdir(path):
            path = os.path.join(path, "index.html")
        if not os.path.isfile(path):
            return super().do_GET()

        if not path.endswith(".html"):
            return super().do_GET()

        try:
            with open(path, "rb") as f:
                data = f.read()
        except OSError:
            return super().do_GET()

        if b"</body>" in data:
            data = data.replace(b"</body>", WIDGET_SNIPPET + b"</body>", 1)

        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        self.wfile.write(data)

    # ── bench-race proxy (no HTML rewrite, no widget injection) ─────
    def _proxy_bench(self, upstream_base: str, prefix: str, method: str, path: str):
        bare, _, query = path.partition("?")
        sub = bare[len(prefix):] or "/"
        upstream_url = upstream_base + sub
        if query:
            upstream_url += "?" + query
        body = None
        if method in ("POST", "PUT"):
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else b""
        req = urllib.request.Request(upstream_url, data=body, method=method)
        for header in ("Content-Type", "Accept"):
            value = self.headers.get(header)
            if value:
                req.add_header(header, value)
        # Stress runs serve back after deadline + every worker drains
        # its in-flight op. At conc=64 on a Python-wrapped SQLite, GIL
        # contention serialises 64 workers × 100k row-tuple creations
        # so the server-side wall can reach ~75s even with a 5s
        # deadline. Cap stress proxy at 120s; other routes (/run,
        # /health, /stats, /workloads) stay snappy at 15s.
        bench_timeout = 120 if sub == "/stress" else 15
        try:
            with urllib.request.urlopen(req, timeout=bench_timeout) as resp:
                status = resp.status
                payload = resp.read()
                ctype = resp.headers.get("Content-Type", "application/json")
        except urllib.error.HTTPError as e:
            status = e.code
            payload = e.read() if hasattr(e, "read") else b""
            ctype = e.headers.get("Content-Type", "application/json") if e.headers else "application/json"
        except Exception as exc:  # noqa: BLE001
            self.send_response(502)
            self.send_header("Content-Type", "application/json")
            msg = json.dumps({"error": "bench_upstream_unreachable", "detail": str(exc)}).encode()
            self.send_header("Content-Length", str(len(msg)))
            self.end_headers()
            self.wfile.write(msg)
            return
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    # ── demo proxy (per-demo entry from DEMOS) ──────────────────────
    def _proxy_demo(self, demo: dict, method: str, path: str):
        prefix = demo["prefix"]
        bare, _, query = path.partition("?")
        sub = bare[len(prefix):]
        if sub.startswith("/api"):
            upstream_base = demo["api"]
            sub = sub[len("/api"):] or "/"
        else:
            upstream_base = demo["static"]
            sub = sub or "/"
        upstream_url = upstream_base + sub
        if query:
            upstream_url += "?" + query

        body = None
        if method in ("POST", "PUT"):
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else b""

        req = urllib.request.Request(upstream_url, data=body, method=method)
        for header in ("Content-Type", "Accept"):
            value = self.headers.get(header)
            if value:
                req.add_header(header, value)

        # Stress runs serve back after deadline + every worker drains its
        # in-flight op — at conc=64 on a Python-wrapped SQLite, GIL
        # contention serialises 64 workers × 100k row-tuple creations
        # so the server-side wall can reach ~75s even with a 5s
        # deadline (measured 1.18s per worker × 64 workers serialised
        # via GIL = 75s). Cap at 120s for /stress so the genuine
        # measurement reaches the client; other proxied routes (/run,
        # /health, /stats) stay on 30s.
        sub_bare = sub.partition("?")[0]
        upstream_timeout = 120 if sub_bare == "/stress" else 30

        try:
            with urllib.request.urlopen(req, timeout=upstream_timeout) as resp:
                status = resp.status
                resp_headers = list(resp.headers.items())
                payload = resp.read()
        except urllib.error.HTTPError as e:
            status = e.code
            resp_headers = list(e.headers.items()) if e.headers else []
            payload = e.read() if hasattr(e, "read") else b""
        except Exception as exc:  # noqa: BLE001
            self.send_response(502)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            msg = f"bad gateway: demo upstream unreachable ({exc})".encode()
            self.send_header("Content-Length", str(len(msg)))
            self.end_headers()
            self.wfile.write(msg)
            return

        ctype = next((v for (k, v) in resp_headers if k.lower() == "content-type"), "")
        if ctype.lower().startswith("text/html"):
            # Per-demo HTML rewrite (e.g. alphafold's hardcoded API URL).
            # Empty rewrite_from = no-op.
            rewrite_from = demo.get("html_api_rewrite_from") or b""
            rewrite_to   = demo.get("html_api_rewrite_to") or b""
            if rewrite_from:
                payload = payload.replace(rewrite_from, rewrite_to)
            # Feedback widget is injected into every demo's HTML too.
            if b"</body>" in payload:
                payload = payload.replace(b"</body>", WIDGET_SNIPPET + b"</body>", 1)

        self.send_response(status)
        skip = {"transfer-encoding", "content-length", "connection"}
        for key, value in resp_headers:
            if key.lower() in skip:
                continue
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        if method != "HEAD":
            self.wfile.write(payload)

    # ── /v1 passthrough (machine wire API, no rewrite) ──────────────
    def _proxy_passthrough(self, upstream_base: str, path: str, method: str):
        body = None
        if method in ("POST", "PUT"):
            n = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(n) if n else b""
        req = urllib.request.Request(upstream_base + path, data=body, method=method)
        for h in ("Content-Type", "Accept"):
            v = self.headers.get(h)
            if v:
                req.add_header(h, v)
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                status, payload = resp.status, resp.read()
                ctype = resp.headers.get("Content-Type", "application/json")
        except urllib.error.HTTPError as e:
            status, payload = e.code, (e.read() if hasattr(e, "read") else b"")
            ctype = "application/json"
        except Exception as exc:  # noqa: BLE001
            return self._json(502, {"error": "v1_upstream_unreachable", "detail": str(exc)})
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        if method != "HEAD":
            self.wfile.write(payload)

    # ── /studio proxy (rewrite /api → /studio/api on HTML) ──────────
    def _proxy_studio(self, sub: str, method: str):
        body = None
        if method in ("POST", "PUT"):
            n = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(n) if n else b""
        req = urllib.request.Request(STUDIO_UPSTREAM + (sub or "/"), data=body, method=method)
        for h in ("Content-Type", "Accept", "Cookie"):
            v = self.headers.get(h)
            if v:
                req.add_header(h, v)
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                status = resp.status
                resp_headers = list(resp.headers.items())
                payload = resp.read()
        except urllib.error.HTTPError as e:
            status = e.code
            resp_headers = list(e.headers.items()) if e.headers else []
            payload = e.read() if hasattr(e, "read") else b""
        except Exception as exc:  # noqa: BLE001
            return self._json(502, {"error": "studio_upstream_unreachable", "detail": str(exc)})
        ctype = next((v for (k, v) in resp_headers if k.lower() == "content-type"), "")
        if ctype.lower().startswith("text/html"):
            payload = payload.replace(b'"/api"', b'"/studio/api"')
        self.send_response(status)
        skip = {"transfer-encoding", "content-length", "connection"}
        for key, value in resp_headers:
            if key.lower() in skip:
                continue
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        if method != "HEAD":
            self.wfile.write(payload)

    def log_message(self, fmt, *args):  # noqa: N802
        sys.stderr.write("%s - - [%s] %s\n" % (
            self.address_string(), self.log_date_time_string(), fmt % args,
        ))


class ThreadingServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def main():
    os.chdir(SITE_ROOT)
    print(f"nDB knowledge site on http://{LISTEN_HOST}:{LISTEN_PORT}", flush=True)
    print(f"  static root:    {SITE_ROOT}", flush=True)
    for d in DEMOS:
        print(f"  demo proxy:     {d['prefix']}/  → {d['static']}", flush=True)
        print(f"                  {d['prefix']}/api/  → {d['api']}", flush=True)
    for prefix, upstream in BENCH_BACKENDS.items():
        print(f"  bench proxy:    {prefix}/  → {upstream}", flush=True)
    print(f"  feedback API:   /api/feedback  → {FEEDBACK_API} (nDB type {FB_TYPE_FEEDBACK})", flush=True)
    print(f"  event log:      {FEEDBACK_EVENT_LOG}", flush=True)
    print(f"  telegram push:  {'enabled' if _tg_configured() else 'disabled (no ~/.claude/channels/telegram/.env or chat_id)'}", flush=True)
    with ThreadingServer((LISTEN_HOST, LISTEN_PORT), Handler) as srv:
        try:
            srv.serve_forever()
        except KeyboardInterrupt:
            print("\nstopping.", flush=True)


if __name__ == "__main__":
    main()
