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

SITE_ROOT = Path(__file__).resolve().parent

# Each demo gets a (static-port, api-port, url-prefix) triple. The Python
# dispatcher iterates DEMOS to route requests. Adding a new demo is a
# single dict entry here + a launcher entry + a narrative page.
DEMOS = [
    {"prefix": "/alphafold_ndb",  "static": "http://127.0.0.1:9876", "api": "http://127.0.0.1:8742",
     "html_api_rewrite_from": b'"http://127.0.0.1:8742"',
     "html_api_rewrite_to":   b'(window.location.origin + "/alphafold_ndb/api")'},
    {"prefix": "/exoplanet_ndb",  "static": "http://127.0.0.1:9877", "api": "http://127.0.0.1:8745",
     # exoplanet SPA detects same-origin itself (location.port check) — no
     # rewrite needed. Empty bytes match nothing, so .replace() is a no-op.
     "html_api_rewrite_from": b'',
     "html_api_rewrite_to":   b''},
    {"prefix": "/seismic_ndb",    "static": "http://127.0.0.1:9878", "api": "http://127.0.0.1:8746",
     "html_api_rewrite_from": b'',
     "html_api_rewrite_to":   b''},
    {"prefix": "/chemistry_ndb",  "static": "http://127.0.0.1:9879", "api": "http://127.0.0.1:8747",
     "html_api_rewrite_from": b'',
     "html_api_rewrite_to":   b''},
    {"prefix": "/biodiv_ndb",     "static": "http://127.0.0.1:9881", "api": "http://127.0.0.1:8748",
     "html_api_rewrite_from": b'',
     "html_api_rewrite_to":   b''},
]

# Back-compat for the old single-demo constants (unused now but kept so
# diffs against earlier server.py read cleanly).
DEMO_STATIC = DEMOS[0]["static"]
DEMO_API = DEMOS[0]["api"]
DEMO_PREFIX = DEMOS[0]["prefix"]

# Upstream for the feedback nDB. Separate engine + DB from the demo.
FEEDBACK_API = "http://127.0.0.1:8744"

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

    def _json(self, status: int, obj):
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

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
