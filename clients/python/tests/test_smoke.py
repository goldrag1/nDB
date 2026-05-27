"""End-to-end smoke tests against a real `ndb-server`.

Skipped by default unless `NDB_PYTHON_SMOKE=1` is set in the env. The
build pipeline can opt in by exporting that env var; local dev runs by
hand.
"""

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import uuid


SHOULD_RUN = os.environ.get("NDB_PYTHON_SMOKE") == "1"


def _find_free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


@unittest.skipUnless(SHOULD_RUN, "set NDB_PYTHON_SMOKE=1 to enable")
class TestRoundTrip(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        from ndb_client import Client

        cls.Client = Client
        cls.port = _find_free_port()
        cls.dbdir = tempfile.mkdtemp(prefix="ndb-pytest-")
        repo_root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
        cls.proc = subprocess.Popen(
            [
                "cargo",
                "run",
                "-q",
                "-p",
                "ndb-server",
                "--",
                "--path",
                cls.dbdir,
                "--bind",
                f"127.0.0.1:{cls.port}",
            ],
            cwd=repo_root,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.time() + 30
        while time.time() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", cls.port), timeout=0.2):
                    break
            except OSError:
                time.sleep(0.2)
        else:
            cls.proc.kill()
            raise RuntimeError("ndb-server did not come up in 30s")
        cls.client = cls.Client(base_url=f"http://127.0.0.1:{cls.port}")

    @classmethod
    def tearDownClass(cls):
        cls.proc.kill()
        cls.proc.wait(timeout=5)

    def test_health(self):
        self.assertEqual(self.client.health(), {"status": "ok"})

    def test_commit_then_read(self):
        eid = str(uuid.uuid4())
        resp = self.client.commit([
            {
                "kind": "entity",
                "entity_id": eid,
                "type_id": 1,
                "tx_id_assert": 0,
                "tx_id_supersede": "active",
                "properties": [
                    {"prop_id": 10, "value": {"tag": "string", "value": "alice"}}
                ],
            }
        ])
        self.assertGreater(resp["tx_id"], 0)
        read = self.client.read(eid)
        self.assertEqual(read["outcome"], "live")
        self.assertEqual(read["record"]["entity_id"], eid)

    def test_iter_streams(self):
        # Iter returns at least the previously-committed entity.
        records = list(self.client.iter())
        self.assertTrue(any(r.get("kind") == "entity" for r in records))


class TestUnitsNoServer(unittest.TestCase):
    """Pure-Python tests that don't need a running server."""

    def test_distance_l2(self):
        from ndb_client.client import _distance

        self.assertEqual(_distance([0, 0, 0], [1, 1, 1], "l2"), 3)

    def test_distance_cosine_unit_aligned(self):
        from ndb_client.client import _distance

        # Same direction → cosine distance ≈ 0.
        d = _distance([1.0, 0.0], [5.0, 0.0], "cosine")
        self.assertAlmostEqual(d, 0.0, places=6)

    def test_distance_cosine_perpendicular(self):
        from ndb_client.client import _distance

        d = _distance([1.0, 0.0], [0.0, 1.0], "cosine")
        self.assertAlmostEqual(d, 1.0, places=6)

    def test_unwrap_value_tagged(self):
        from ndb_client.client import _unwrap_value

        self.assertEqual(_unwrap_value({"tag": "string", "value": "x"}), "x")
        self.assertIsNone(_unwrap_value(None))
        self.assertEqual(_unwrap_value(42), 42)

    def test_http_error_raised_with_structured_body(self):
        from ndb_client import NdbHttpError

        try:
            raise NdbHttpError(401, "unauthorized", "missing token")
        except NdbHttpError as e:
            self.assertEqual(e.status, 401)
            self.assertEqual(e.error, "unauthorized")
            self.assertEqual(e.detail, "missing token")
            self.assertIn("missing token", str(e))


if __name__ == "__main__":
    unittest.main()
