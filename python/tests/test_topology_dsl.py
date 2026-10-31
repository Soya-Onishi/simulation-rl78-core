"""CLI and ELF-embedding tests for topology_dsl."""

from __future__ import annotations

import json
import os
import subprocess
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory

from topology_dsl import embed_local_elfs, main, resolve_server


class ResolveServerTests(unittest.TestCase):
    def test_flag_overrides_env_and_default(self) -> None:
        env = {"SIMULATION_SERVER": "http://env.example:9"}
        self.assertEqual(
            resolve_server("http://flag.example:8/", env),
            "http://flag.example:8",
        )
        self.assertEqual(resolve_server(None, env), "http://env.example:9")
        self.assertEqual(resolve_server(None, {}), "http://127.0.0.1:8090")


class EmbedLocalElfsTests(unittest.TestCase):
    def test_relative_path_uses_cwd(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "fw.bin").write_bytes(b"ABC")
            topo = {
                "boards": [
                    {"id": "a", "elf": "fw.bin"},
                    {"id": "b", "elf": "https://example.test/fw.elf"},
                    {"id": "c", "elf": "base64:AQI="},
                ]
            }
            prev = os.getcwd()
            os.chdir(root)
            try:
                out = embed_local_elfs(topo)
            finally:
                os.chdir(prev)
            self.assertEqual(out["boards"][0]["elf"], "base64:QUJD")
            self.assertEqual(out["boards"][1]["elf"], "https://example.test/fw.elf")
            self.assertEqual(out["boards"][2]["elf"], "base64:AQI=")

    def test_file_url_and_missing_file(self) -> None:
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "fw.bin"
            path.write_bytes(b"Z")
            file_url = path.resolve().as_uri()
            out = embed_local_elfs({"boards": [{"id": "a", "elf": file_url}]})
            self.assertTrue(out["boards"][0]["elf"].startswith("base64:"))
            with self.assertRaises(SystemExit) as raised:
                embed_local_elfs({"boards": [{"id": "a", "elf": str(path / "missing")}]})
            self.assertEqual(raised.exception.code, 1)


class _CreateHandler(BaseHTTPRequestHandler):
    last_body: dict | None = None

    def log_message(self, fmt: str, *args: object) -> None:
        return

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        type(self).last_body = json.loads(raw.decode("utf-8"))
        self.send_response(201)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"id":"test-id"}')


class CreateCliTests(unittest.TestCase):
    def test_usage_is_exit_2(self) -> None:
        with self.assertRaises(SystemExit) as raised:
            main([])
        self.assertEqual(raised.exception.code, 2)
        with self.assertRaises(SystemExit) as raised:
            main(["create"])
        self.assertEqual(raised.exception.code, 2)
        with self.assertRaises(SystemExit) as raised:
            main(["create", "--server"])
        self.assertEqual(raised.exception.code, 2)
        with self.assertRaises(SystemExit) as raised:
            main(["nope", "x.py"])
        self.assertEqual(raised.exception.code, 2)

    def test_create_posts_and_prints_url(self) -> None:
        server = HTTPServer(("127.0.0.1", 0), _CreateHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            host, port = server.server_address
            origin = f"http://{host}:{port}"
            with TemporaryDirectory() as tmp:
                root = Path(tmp)
                (root / "fw.bin").write_bytes(b"ABC")
                script = root / "topo.py"
                script.write_text(
                    "from topology_dsl import Topology, emit\n"
                    "t = Topology(margin_ns=1, headroom_threshold_ns=1)\n"
                    't.board("a", elf="fw.bin")\n'
                    "emit(t)\n",
                    encoding="utf-8",
                )
                prev = os.getcwd()
                os.chdir(root)
                try:
                    from io import StringIO
                    import sys

                    buf = StringIO()
                    old = sys.stdout
                    sys.stdout = buf
                    try:
                        main(["create", str(script), "--server", origin])
                    finally:
                        sys.stdout = old
                finally:
                    os.chdir(prev)
            self.assertEqual(buf.getvalue().strip(), f"{origin}/test-id")
            body = _CreateHandler.last_body
            assert body is not None
            self.assertEqual(body["topology"]["boards"][0]["elf"], "base64:QUJD")
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)


class CreateSimScriptTests(unittest.TestCase):
    def _script(self) -> Path:
        return Path(__file__).resolve().parents[2] / "scripts" / "create-sim.sh"

    def _run(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(self._script()), *args],
            capture_output=True,
            text=True,
        )

    def test_empty_args_is_shell_error(self) -> None:
        result = self._run()
        self.assertEqual(result.returncode, 2)
        self.assertIn("missing topology script", result.stderr)
        self.assertNotIn("topology_dsl", result.stderr)

    def test_non_python_path_is_shell_error(self) -> None:
        result = self._run("topology.json")
        self.assertEqual(result.returncode, 2)
        self.assertIn("must be a .py path", result.stderr)
        self.assertNotIn("topology_dsl", result.stderr)

    def test_flags_without_script_is_shell_error(self) -> None:
        result = self._run("--open")
        self.assertEqual(result.returncode, 2)
        self.assertIn("missing topology script", result.stderr)
        self.assertNotIn("topology_dsl", result.stderr)


if __name__ == "__main__":
    unittest.main()
