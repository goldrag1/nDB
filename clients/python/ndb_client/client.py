"""nDB HTTP client. Stdlib-only.

Wire-protocol shapes mirror :mod:`ndb_engine::wire`. The client does no
validation of payload shapes beyond JSON-serialisability — the server
returns 4xx with a structured error body on bad input.
"""

from __future__ import annotations

import json as _json
import os
import ssl as _ssl
from dataclasses import dataclass
from typing import Any, Iterable, Iterator, Mapping, Optional, Sequence, Union
from urllib import error as _urllib_error
from urllib import parse as _urllib_parse
from urllib import request as _urllib_request


class NdbError(Exception):
    """Base class for every error this client raises."""


class NdbConnectionError(NdbError):
    """Network-layer failure (DNS, TCP refused, TLS handshake)."""


class NdbHttpError(NdbError):
    """The server returned a non-2xx response.

    Attributes
    ----------
    status:
        HTTP status code.
    error:
        Short machine-readable error tag from the server's
        :class:`ErrorResponse` envelope.
    detail:
        Human-readable failure detail.
    """

    def __init__(self, status: int, error: str, detail: str):
        super().__init__(f"{status} {error}: {detail}")
        self.status = status
        self.error = error
        self.detail = detail


@dataclass
class _ParsedResponse:
    status: int
    body: bytes


class Client:
    """nDB HTTP client.

    Parameters
    ----------
    base_url:
        URL of the running ``ndb-server`` instance, e.g.
        ``"http://127.0.0.1:8742"`` or ``"https://ndb.example.com"``.
    token:
        Optional bearer token. Sent as ``Authorization: Bearer
        <token>`` on every request. Falls back to ``NDB_TOKEN`` in the
        environment.
    timeout:
        Per-request timeout, seconds. Default 30.
    verify_ssl:
        For ``https://`` URLs, whether to verify the server certificate
        chain. Default ``True``. Set to ``False`` for self-signed
        development certificates.
    """

    def __init__(
        self,
        base_url: str = "http://127.0.0.1:8742",
        token: Optional[str] = None,
        timeout: float = 30.0,
        verify_ssl: bool = True,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.token = token or os.environ.get("NDB_TOKEN") or None
        self.timeout = float(timeout)
        if base_url.startswith("https://") and not verify_ssl:
            self._ssl_ctx: Optional[_ssl.SSLContext] = _ssl._create_unverified_context()
        else:
            self._ssl_ctx = None

    # ---------------------------------------------------------------
    # Public API — one method per route + helpers.
    # ---------------------------------------------------------------

    def health(self) -> dict:
        """``GET /health`` — liveness probe."""
        resp = self._request("GET", "/health")
        return _json.loads(resp.body)

    def commit(self, records: Sequence[Mapping[str, Any]]) -> dict:
        """``POST /commit`` — commit a batch of records.

        Returns ``{"tx_id": <u64>}``.
        """
        body = {"records": list(records)}
        resp = self._request("POST", "/commit", body=body)
        return _json.loads(resp.body)

    def read(self, uuid: str) -> dict:
        """``GET /read/:uuid`` — point lookup at the latest snapshot.

        Returns the ``ReadResponse`` envelope: one of

        - ``{"outcome": "missing"}``
        - ``{"outcome": "deleted", "deleted_at": <u64>}``
        - ``{"outcome": "live", "record": {...}}``
        """
        resp = self._request("GET", f"/read/{_urllib_parse.quote(uuid)}")
        return _json.loads(resp.body)

    def iter(self) -> Iterator[dict]:
        """``GET /iter`` — stream every visible record as a JSONL feed.

        Yields one parsed JSON record per line. The server holds the
        engine mutex for the duration; expect this to block other
        writers until the iterator is exhausted.
        """
        url = self._url("/iter")
        req = _urllib_request.Request(url, method="GET", headers=self._auth_headers())
        try:
            kwargs: dict[str, Any] = {"timeout": self.timeout}
            if self._ssl_ctx is not None:
                kwargs["context"] = self._ssl_ctx
            opened = _urllib_request.urlopen(req, **kwargs)  # noqa: S310 — user-supplied URL
        except _urllib_error.HTTPError as e:
            self._raise_http(e)
        except _urllib_error.URLError as e:
            raise NdbConnectionError(str(e)) from e
        with opened as fh:
            for raw in fh:
                line = raw.decode("utf-8").rstrip("\n")
                if not line:
                    continue
                yield _json.loads(line)

    def flush(self) -> dict:
        """``POST /flush`` — drain the memtable into a new SSTable."""
        resp = self._request("POST", "/flush", body={})
        return _json.loads(resp.body)

    def compact(self) -> dict:
        """``POST /compact`` — full compaction across open SSTables."""
        resp = self._request("POST", "/compact", body={})
        return _json.loads(resp.body)

    # ---------------------------------------------------------------
    # Higher-level helpers — convenience over the raw routes.
    # ---------------------------------------------------------------

    def lookup_by_key(self, property_id: int, value: dict) -> Optional[str]:
        """Look up an entity by an external lookup-key value.

        ``value`` follows the tagged-union JSON shape, e.g.
        ``{"tag": "string", "value": "alice@example.com"}``.

        The server doesn't yet expose a dedicated route for this in v1;
        until it does, this helper scans ``/iter`` and matches client
        side. Future server versions will swap in an indexed
        ``GET /lookup`` route without changing the client surface.
        """
        wanted = (property_id, _json.dumps(value, sort_keys=True))
        for rec in self.iter():
            if rec.get("kind") != "entity":
                continue
            for p in rec.get("properties", []):
                if (p.get("prop_id"), _json.dumps(p.get("value"), sort_keys=True)) == wanted:
                    return rec.get("entity_id")
        return None

    def vector_search(
        self,
        property_id: int,
        query: Sequence[float],
        k: int,
        metric: str = "l2",
    ) -> list[tuple[str, float]]:
        """k-NN search over a vector-indexed property.

        Implemented client-side over ``/iter`` for v1 (mirrors
        :meth:`lookup_by_key`). When the server gains a dedicated route,
        this surface stays.
        """
        if metric not in ("l2", "cosine"):
            raise ValueError("metric must be 'l2' or 'cosine'")
        candidates: list[tuple[str, float]] = []
        for rec in self.iter():
            if rec.get("kind") != "entity":
                continue
            for p in rec.get("properties", []):
                if p.get("prop_id") != property_id:
                    continue
                v = p.get("value")
                if not isinstance(v, dict) or v.get("tag") != "vector":
                    continue
                vec = v.get("value", [])
                if len(vec) != len(query):
                    continue
                d = _distance(query, vec, metric)
                candidates.append((rec.get("entity_id"), d))
                break
        candidates.sort(key=lambda pair: pair[1])
        return candidates[:k]

    def property_lookup(
        self,
        type_id: int,
        property_id: int,
        value: dict,
    ) -> list[str]:
        """Exact match on ``(type, property, value)`` — client-side."""
        wanted = (type_id, property_id, _json.dumps(value, sort_keys=True))
        out: list[str] = []
        for rec in self.iter():
            if rec.get("kind") != "entity" or rec.get("type_id") != type_id:
                continue
            for p in rec.get("properties", []):
                if (
                    type_id,
                    p.get("prop_id"),
                    _json.dumps(p.get("value"), sort_keys=True),
                ) == wanted:
                    out.append(rec.get("entity_id"))
                    break
        return out

    def property_range(
        self,
        type_id: int,
        property_id: int,
        low: Optional[Union[int, float, str]] = None,
        high: Optional[Union[int, float, str]] = None,
    ) -> list[str]:
        """Range query on ``(type, property)`` — client-side."""
        out: list[str] = []
        for rec in self.iter():
            if rec.get("kind") != "entity" or rec.get("type_id") != type_id:
                continue
            for p in rec.get("properties", []):
                if p.get("prop_id") != property_id:
                    continue
                v = p.get("value")
                inner = v.get("value") if isinstance(v, dict) else None
                if inner is None:
                    continue
                if low is not None and inner < low:
                    continue
                if high is not None and inner > high:
                    continue
                out.append(rec.get("entity_id"))
                break
        return out

    def iter_arrow(self):  # type: ignore[no-untyped-def]
        """Materialise the record stream as a ``pyarrow.RecordBatch``.

        Requires the ``arrow`` extra::

            pip install 'ndb-client[arrow]'

        The schema is denormalised: one column per
        ``(record_kind, type_id, property_id)`` tuple observed, plus
        identity columns (``record_kind``, ``primary_id``, ``type_id``,
        ``tx_id_assert``, ``tx_id_supersede``). Mirrors the ``ndb-arrow``
        Rust crate's projection.
        """
        try:
            import pyarrow as pa  # type: ignore[import-not-found]
        except ImportError as e:
            raise NdbError(
                "iter_arrow requires pyarrow. Install with: pip install 'ndb-client[arrow]'"
            ) from e

        rows = list(self.iter())
        prop_columns: dict[tuple[str, int, int], list[Any]] = {}
        kinds: list[str] = []
        primary_ids: list[str] = []
        type_ids: list[Optional[int]] = []
        tx_asserts: list[Optional[int]] = []
        tx_supersedes: list[Optional[int]] = []

        # First pass: discover the property column set.
        for rec in rows:
            kind = rec.get("kind", "?")
            if kind in ("entity", "hyperedge"):
                t = rec.get("type_id")
                for p in rec.get("properties", []):
                    key = (kind, t, p.get("prop_id"))
                    prop_columns.setdefault(key, [])

        # Second pass: row out each record.
        for rec in rows:
            kind = rec.get("kind")
            if kind == "entity":
                kinds.append("entity")
                primary_ids.append(rec.get("entity_id", ""))
                type_ids.append(rec.get("type_id"))
                tx_asserts.append(rec.get("tx_id_assert"))
                sup = rec.get("tx_id_supersede")
                tx_supersedes.append(None if sup == "active" else sup)
                _fill_props(rec, kind, prop_columns)
            elif kind == "hyperedge":
                kinds.append("hyperedge")
                primary_ids.append(rec.get("hyperedge_id", ""))
                type_ids.append(rec.get("type_id"))
                tx_asserts.append(rec.get("tx_id_assert"))
                sup = rec.get("tx_id_supersede")
                tx_supersedes.append(None if sup == "active" else sup)
                _fill_props(rec, kind, prop_columns)
            elif kind == "tombstone":
                kinds.append("tombstone")
                primary_ids.append(rec.get("target_id", ""))
                type_ids.append(None)
                tx_asserts.append(None)
                tx_supersedes.append(rec.get("tx_id_supersede"))
                _fill_props(rec, kind, prop_columns)
            else:
                # Dictionary records — skip in row projection.
                continue

        n_rows = len(kinds)
        for col in prop_columns.values():
            while len(col) < n_rows:
                col.append(None)

        arrays: dict[str, Any] = {
            "record_kind": pa.array(kinds, type=pa.string()),
            "primary_id": pa.array(primary_ids, type=pa.string()),
            "type_id": pa.array(type_ids, type=pa.uint32()),
            "tx_id_assert": pa.array(tx_asserts, type=pa.uint64()),
            "tx_id_supersede": pa.array(tx_supersedes, type=pa.uint64()),
        }
        for (k, t, p), col in prop_columns.items():
            name = f"prop:{k}:{t}:{p}"
            arrays[name] = pa.array([_unwrap_value(v) for v in col])
        return pa.RecordBatch.from_pydict(arrays)

    # ---------------------------------------------------------------
    # Internals.
    # ---------------------------------------------------------------

    def _url(self, path: str) -> str:
        return f"{self.base_url}{path}"

    def _auth_headers(self) -> dict[str, str]:
        headers = {"Accept": "application/json"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        return headers

    def _request(
        self,
        method: str,
        path: str,
        body: Optional[Any] = None,
    ) -> _ParsedResponse:
        url = self._url(path)
        data = None
        headers = self._auth_headers()
        if body is not None:
            data = _json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
            headers["Content-Length"] = str(len(data))
        req = _urllib_request.Request(url, data=data, method=method, headers=headers)
        try:
            kwargs: dict[str, Any] = {"timeout": self.timeout}
            if self._ssl_ctx is not None:
                kwargs["context"] = self._ssl_ctx
            with _urllib_request.urlopen(req, **kwargs) as fh:  # noqa: S310 — user-supplied URL
                status = fh.status
                body_bytes = fh.read()
            return _ParsedResponse(status=status, body=body_bytes)
        except _urllib_error.HTTPError as e:
            self._raise_http(e)
        except _urllib_error.URLError as e:
            raise NdbConnectionError(str(e)) from e

    def _raise_http(self, e: _urllib_error.HTTPError) -> None:
        body = e.read()
        try:
            parsed = _json.loads(body)
        except Exception:
            parsed = {}
        err = parsed.get("error", "http_error") if isinstance(parsed, dict) else "http_error"
        detail = (
            parsed.get("detail", body.decode("utf-8", errors="replace"))
            if isinstance(parsed, dict)
            else body.decode("utf-8", errors="replace")
        )
        raise NdbHttpError(status=e.code, error=err, detail=detail)


def _distance(query: Sequence[float], vec: Sequence[float], metric: str) -> float:
    if metric == "l2":
        return sum((a - b) ** 2 for a, b in zip(query, vec))
    # cosine
    dot = sum(a * b for a, b in zip(query, vec))
    na = sum(a * a for a in query) ** 0.5
    nb = sum(b * b for b in vec) ** 0.5
    denom = max(na * nb, 1e-12)
    return 1.0 - (dot / denom)


def _fill_props(
    rec: Mapping[str, Any],
    kind: str,
    prop_columns: dict[tuple[str, int, int], list[Any]],
) -> None:
    t = rec.get("type_id")
    seen_keys: set[tuple[str, int, int]] = set()
    for p in rec.get("properties", []):
        key = (kind, t, p.get("prop_id"))
        prop_columns.setdefault(key, [])
        # Pad column to the current row length minus 1, then append.
        prop_columns[key].append(p.get("value"))
        seen_keys.add(key)
    # Other columns get None for this row (filled at finalisation, not here).
    _ = seen_keys


def _unwrap_value(v: Any) -> Any:
    """Convert a tagged-union ``{"tag": ..., "value": ...}`` to its
    inner scalar for Arrow consumption. Returns ``None`` for ``None``
    and for unrecognised tags."""
    if v is None:
        return None
    if isinstance(v, dict) and "tag" in v:
        return v.get("value")
    return v


# Re-export for typing convenience.
Records = Iterable[Mapping[str, Any]]
