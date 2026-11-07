from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from topology_dsl import SimulationConfig, Topology

from systemtest.simulation import Simulation


class _FakeViewer:
    def __init__(self, *_args, **_kwargs) -> None:
        self.closed = False

    def start(self) -> str:
        return "token"

    def close(self) -> None:
        self.closed = True


class _FakeClient:
    def __init__(self, *_args, **_kwargs) -> None:
        self.base_url = "http://test"
        self.created: list[str] = []
        self.topologies: list[dict] = []
        self.deleted: list[str] = []
        self.closed = False
        self.csv_calls = 0
        self.fail_catalog = False
        self.fail_delete = False

    def create(self, topology: dict) -> str:
        self.topologies.append(topology)
        self.created.append("s1")
        return "s1"

    def set_token(self, _token: str) -> None:
        return None

    def catalog(self, _sim_id: str | None) -> list:
        if self.fail_catalog:
            raise RuntimeError("catalog boom")
        return []

    def delete(self, sim_id: str) -> None:
        self.deleted.append(sim_id)
        if self.fail_delete:
            raise RuntimeError("delete boom")

    def csv(self, _sim_id: str) -> bytes:
        self.csv_calls += 1
        return b""

    def close(self) -> None:
        self.closed = True


def _empty_topology() -> dict:
    return {
        "boards": [],
        "edges": [],
        "margin_ns": 1,
        "headroom_threshold_ns": 1,
    }


class SimulationInitTests(unittest.TestCase):
    def test_create_embeds_local_elf_as_base64(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            elf = Path(tmp) / "fw.bin"
            elf.write_bytes(b"ABC")
            client = _FakeClient()

            class T(Topology):
                def build(self) -> None:
                    self.board("a", elf=str(elf))

            def config() -> SimulationConfig:
                return SimulationConfig(
                    margin_ns=1,
                    headroom_threshold_ns=1,
                    topology=T(name="demo"),
                )

            with (
                patch("systemtest.simulation.ClusterClient", lambda *_a, **_k: client),
                patch("systemtest.simulation.ViewerHold", _FakeViewer),
            ):
                Simulation(topology=config().to_dict(), csv=False, testname="t")

            self.assertEqual(client.topologies[0]["boards"][0]["elf"], "base64:QUJD")
            self.assertEqual(client.topologies[0]["boards"][0]["id"], "demo/a")

    def test_catalog_failure_deletes_created_simulation(self) -> None:
        client = _FakeClient()
        client.fail_catalog = True
        viewers: list[_FakeViewer] = []

        class TrackingViewer(_FakeViewer):
            def __init__(self, *args, **kwargs) -> None:
                super().__init__(*args, **kwargs)
                viewers.append(self)

        with (
            patch("systemtest.simulation.ClusterClient", lambda *_a, **_k: client),
            patch("systemtest.simulation.ViewerHold", TrackingViewer),
        ):
            with self.assertRaisesRegex(RuntimeError, "catalog boom"):
                Simulation(topology=_empty_topology(), csv=False, testname="t")

        self.assertEqual(client.created, ["s1"])
        self.assertEqual(client.deleted, ["s1"])
        self.assertTrue(client.closed)
        self.assertTrue(viewers[0].closed)

    def test_viewer_start_failure_deletes_created_simulation(self) -> None:
        client = _FakeClient()

        class BoomViewer(_FakeViewer):
            def start(self) -> str:
                raise RuntimeError("viewer boom")

        with (
            patch("systemtest.simulation.ClusterClient", lambda *_a, **_k: client),
            patch("systemtest.simulation.ViewerHold", BoomViewer),
        ):
            with self.assertRaisesRegex(RuntimeError, "viewer boom"):
                Simulation(topology=_empty_topology(), csv=False, testname="t")

        self.assertEqual(client.deleted, ["s1"])
        self.assertTrue(client.closed)

    def test_setup_failure_does_not_write_csv(self) -> None:
        client = _FakeClient()
        client.fail_catalog = True
        with (
            patch("systemtest.simulation.ClusterClient", lambda *_a, **_k: client),
            patch("systemtest.simulation.ViewerHold", _FakeViewer),
        ):
            with self.assertRaisesRegex(RuntimeError, "catalog boom"):
                Simulation(topology=_empty_topology(), csv=True, testname="t")

        self.assertEqual(client.csv_calls, 0)
        self.assertEqual(client.deleted, ["s1"])

    def test_setup_failure_keeps_original_error_if_delete_fails(self) -> None:
        client = _FakeClient()
        client.fail_catalog = True
        client.fail_delete = True
        with (
            patch("systemtest.simulation.ClusterClient", lambda *_a, **_k: client),
            patch("systemtest.simulation.ViewerHold", _FakeViewer),
        ):
            with self.assertRaisesRegex(RuntimeError, "catalog boom"):
                Simulation(topology=_empty_topology(), csv=False, testname="t")

        self.assertEqual(client.deleted, ["s1"])
        self.assertTrue(client.closed)


if __name__ == "__main__":
    unittest.main()
