from __future__ import annotations

from types import SimpleNamespace

import pytest

from topology_dsl import Topology

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


def _topology():
    return SimpleNamespace(to_dict=lambda: {})


def _patch_harness(monkeypatch, client: _FakeClient, viewer_cls=None) -> None:
    monkeypatch.setattr("systemtest.simulation.ClusterClient", lambda *_a, **_k: client)
    monkeypatch.setattr("systemtest.simulation.ViewerHold", viewer_cls or _FakeViewer)


def test_create_embeds_local_elf_as_base64(monkeypatch, tmp_path) -> None:
    elf = tmp_path / "fw.bin"
    elf.write_bytes(b"ABC")
    client = _FakeClient()
    _patch_harness(monkeypatch, client)

    topology = Topology(margin_ns=1, headroom_threshold_ns=1)
    topology.board("a", elf=str(elf))
    Simulation(topology=topology, csv=False, testname="t")

    assert client.topologies[0]["boards"][0]["elf"] == "base64:QUJD"


def test_catalog_failure_deletes_created_simulation(monkeypatch) -> None:
    client = _FakeClient()
    client.fail_catalog = True
    viewers: list[_FakeViewer] = []

    class TrackingViewer(_FakeViewer):
        def __init__(self, *args, **kwargs) -> None:
            super().__init__(*args, **kwargs)
            viewers.append(self)

    _patch_harness(monkeypatch, client, TrackingViewer)

    with pytest.raises(RuntimeError, match="catalog boom"):
        Simulation(topology=_topology(), csv=False, testname="t")  # type: ignore[arg-type]

    assert client.created == ["s1"]
    assert client.deleted == ["s1"]
    assert client.closed is True
    assert viewers[0].closed is True


def test_viewer_start_failure_deletes_created_simulation(monkeypatch) -> None:
    client = _FakeClient()

    class BoomViewer(_FakeViewer):
        def start(self) -> str:
            raise RuntimeError("viewer boom")

    _patch_harness(monkeypatch, client, BoomViewer)

    with pytest.raises(RuntimeError, match="viewer boom"):
        Simulation(topology=_topology(), csv=False, testname="t")  # type: ignore[arg-type]

    assert client.deleted == ["s1"]
    assert client.closed is True


def test_setup_failure_does_not_write_csv(monkeypatch) -> None:
    client = _FakeClient()
    client.fail_catalog = True
    _patch_harness(monkeypatch, client)

    with pytest.raises(RuntimeError, match="catalog boom"):
        Simulation(topology=_topology(), csv=True, testname="t")  # type: ignore[arg-type]

    assert client.csv_calls == 0
    assert client.deleted == ["s1"]


def test_setup_failure_keeps_original_error_if_delete_fails(monkeypatch) -> None:
    client = _FakeClient()
    client.fail_catalog = True
    client.fail_delete = True
    _patch_harness(monkeypatch, client)

    with pytest.raises(RuntimeError, match="catalog boom"):
        Simulation(topology=_topology(), csv=False, testname="t")  # type: ignore[arg-type]

    assert client.deleted == ["s1"]
    assert client.closed is True
