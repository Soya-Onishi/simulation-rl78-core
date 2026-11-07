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

from topology_dsl import _load_config_dict, embed_local_elfs, main, resolve_server


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
                    "from topology_dsl import SimulationConfig, Topology, simulation\n"
                    "\n"
                    "class T(Topology):\n"
                    "    def build(self):\n"
                    '        self._board("a", elf="fw.bin")\n'
                    "\n"
                    "@simulation\n"
                    "def demo() -> SimulationConfig:\n"
                    "    return SimulationConfig(\n"
                    "        margin_ns=1,\n"
                    "        headroom_threshold_ns=1,\n"
                    '        topology=T(name="demo"),\n'
                    "    )\n",
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


class SimulationDiscoveryTests(unittest.TestCase):
    def _write(self, root: Path, body: str) -> Path:
        script = root / "topo.py"
        script.write_text(body, encoding="utf-8")
        return script

    def test_rejects_zero_simulation_functions(self) -> None:
        with TemporaryDirectory() as tmp:
            script = self._write(Path(tmp), "x = 1\n")
            with self.assertRaises(SystemExit) as raised:
                _load_config_dict(str(script))
            self.assertIn("no @simulation function", str(raised.exception))

    def test_rejects_two_simulation_functions(self) -> None:
        body = (
            "from topology_dsl import SimulationConfig, Topology, simulation\n"
            "class T(Topology):\n"
            "    def build(self):\n"
            '        self._board("a")\n'
            "@simulation\n"
            "def one() -> SimulationConfig:\n"
            "    return SimulationConfig(1, 1, T(name='demo'))\n"
            "@simulation\n"
            "def two() -> SimulationConfig:\n"
            "    return SimulationConfig(1, 1, T(name='other'))\n"
        )
        with TemporaryDirectory() as tmp:
            script = self._write(Path(tmp), body)
            with self.assertRaises(SystemExit) as raised:
                _load_config_dict(str(script))
            self.assertIn("2 @simulation functions", str(raised.exception))

    def test_rejects_non_config_return(self) -> None:
        body = (
            "from topology_dsl import simulation\n"
            "@simulation\n"
            "def bad():\n"
            "    return 1\n"
        )
        with TemporaryDirectory() as tmp:
            script = self._write(Path(tmp), body)
            with self.assertRaises(SystemExit) as raised:
                _load_config_dict(str(script))
            self.assertIn("must return SimulationConfig", str(raised.exception))

    def test_ignores_imported_simulation_factories(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            helper = root / "helper.py"
            helper.write_text(
                "from topology_dsl import SimulationConfig, Topology, simulation\n"
                "class T(Topology):\n"
                "    def build(self):\n"
                '        self._board("a")\n'
                "@simulation\n"
                "def imported() -> SimulationConfig:\n"
                "    return SimulationConfig(1, 1, T(name='imp'))\n",
                encoding="utf-8",
            )
            script = root / "topo.py"
            script.write_text(
                "import sys\n"
                f"sys.path.insert(0, {str(root)!r})\n"
                "from helper import imported  # noqa: F401\n"
                "from topology_dsl import SimulationConfig, Topology, simulation\n"
                "class T(Topology):\n"
                "    def build(self):\n"
                '        self._board("a")\n'
                "@simulation\n"
                "def local() -> SimulationConfig:\n"
                "    return SimulationConfig(1, 1, T(name='demo'))\n",
                encoding="utf-8",
            )
            out = _load_config_dict(str(script))
            self.assertEqual(out["boards"][0]["id"], "demo/a")


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
