from __future__ import annotations

from typing import Any

import pytest

from systemtest.duration import Duration
from systemtest.errors import WaitError
from systemtest.simulation import Simulation


class _Clock:
    def __init__(self) -> None:
        self.t = 0.0

    def monotonic(self) -> float:
        return self.t

    def sleep(self, seconds: float) -> None:
        self.t += seconds


class _StatusClient:
    def __init__(self, replies: list[dict[str, Any]]) -> None:
        self.replies = replies
        self.i = 0
        self.started: list[int | None] = []

    def start(self, _sim_id: str | None, until_ns: int | None) -> None:
        self.started.append(until_ns)

    def status(self, _sim_id: str | None) -> dict[str, Any]:
        index = min(self.i, len(self.replies) - 1)
        self.i += 1
        return self.replies[index]


def _sim(client: _StatusClient) -> Simulation:
    sim = Simulation.__new__(Simulation)
    sim.virtual_time_ns = 0
    sim._id = "s1"
    sim._client = client  # type: ignore[assignment]
    return sim


def _patch_clock(monkeypatch, clock: _Clock) -> None:
    monkeypatch.setattr("systemtest.simulation.time.monotonic", clock.monotonic)
    monkeypatch.setattr("systemtest.simulation.time.sleep", clock.sleep)


def test_stale_stopped_is_ignored_until_until_ns(monkeypatch) -> None:
    clock = _Clock()
    _patch_clock(monkeypatch, clock)
    client = _StatusClient(
        [
            {"state": "stopped", "virtual_time_ns": 0, "stop_reason": None},
            {"state": "stopped", "virtual_time_ns": 0, "stop_reason": None},
            {"state": "running", "virtual_time_ns": 10},
            {"state": "running", "virtual_time_ns": 500},
            {"state": "stopped", "virtual_time_ns": 1000, "stop_reason": "until"},
        ]
    )
    sim = _sim(client)
    sim.wait(Duration.ns(1000))
    assert sim.virtual_time_ns == 1000
    assert client.started == [1000]


def test_frozen_virtual_time_raises_after_stall(monkeypatch) -> None:
    clock = _Clock()
    _patch_clock(monkeypatch, clock)
    client = _StatusClient([{"state": "running", "virtual_time_ns": 0}])
    sim = _sim(client)
    with pytest.raises(WaitError, match="virtual time did not advance"):
        sim.wait(Duration.ns(1000))
    assert clock.t >= 10.0


def test_overdue_running_raises_after_stall(monkeypatch) -> None:
    clock = _Clock()
    _patch_clock(monkeypatch, clock)
    client = _StatusClient([{"state": "running", "virtual_time_ns": 5000}])
    sim = _sim(client)
    with pytest.raises(WaitError, match="did not stop after until_ns"):
        sim.wait(Duration.ns(1000))
    assert clock.t >= 10.0


def test_long_wait_does_not_false_timeout(monkeypatch) -> None:
    clock = _Clock()
    _patch_clock(monkeypatch, clock)
    vt = {"n": 0}

    class Advancing(_StatusClient):
        def __init__(self) -> None:
            super().__init__([])

        def status(self, _sim_id: str | None) -> dict[str, Any]:
            vt["n"] += 1
            if vt["n"] >= 2000:
                return {"state": "stopped", "virtual_time_ns": vt["n"], "stop_reason": "until"}
            return {"state": "running", "virtual_time_ns": vt["n"]}

    client = Advancing()
    sim = _sim(client)
    sim.wait(Duration.ns(2000))
    assert clock.t > 120.0
    assert sim.virtual_time_ns >= 2000


def test_stall_timer_does_not_count_as_overdue(monkeypatch) -> None:
    clock = _Clock()
    _patch_clock(monkeypatch, clock)
    client = _StatusClient(
        [{"state": "running", "virtual_time_ns": 0}] * 99
        + [{"state": "running", "virtual_time_ns": 1000}] * 4
        + [{"state": "stopped", "virtual_time_ns": 1000, "stop_reason": "until"}]
    )
    sim = _sim(client)
    sim.wait(Duration.ns(1000))
    assert sim.virtual_time_ns == 1000


def test_exited_raises(monkeypatch) -> None:
    clock = _Clock()
    _patch_clock(monkeypatch, clock)
    client = _StatusClient(
        [{"state": "exited", "virtual_time_ns": 0, "stop_reason": "fault"}]
    )
    sim = _sim(client)
    with pytest.raises(WaitError, match="exited"):
        sim.wait(Duration.ns(1000))
