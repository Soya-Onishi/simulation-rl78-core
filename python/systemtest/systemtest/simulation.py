"""Simulation handle injected into @sim_case tests."""

from __future__ import annotations

import os
import re
import time
from collections.abc import Callable
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from topology_dsl import Topology

from systemtest.client import ClusterClient, ViewerHold, default_base_url
from systemtest.duration import Duration
from systemtest.errors import ApiError, ProbeKindError, TruncatedError, WaitError
from systemtest.uart import UartStream

OUT_ENV = "SYSTEMTEST_OUT"
CATALOG_TIMEOUT_S = 15.0
WAIT_TIMEOUT_S = 120.0


def out_dir() -> Path:
    override = os.environ.get(OUT_ENV)
    if override:
        return Path(override)
    return Path.cwd() / "out"


def safe_testname(name: str) -> str:
    return re.sub(r"[^\w.-]+", "_", name)


class DigitalHandle:
    def __init__(self, sim: Simulation, board: str, probe: str) -> None:
        self._sim = sim
        self._board = board
        self._probe = probe

    def latest(self) -> str | None:
        sample = self._sim._latest(self._board, self._probe, "digital")
        if sample is None:
            return None
        value = sample["value"]
        if not isinstance(value, str):
            raise ProbeKindError(f"{self._board}/{self._probe} digital value was {value!r}")
        return value


class AnalogHandle:
    def __init__(self, sim: Simulation, board: str, probe: str) -> None:
        self._sim = sim
        self._board = board
        self._probe = probe

    def latest(self) -> int | None:
        sample = self._sim._latest(self._board, self._probe, "analog")
        if sample is None:
            return None
        value = sample["value"]
        if isinstance(value, bool) or not isinstance(value, int):
            raise ProbeKindError(f"{self._board}/{self._probe} analog value was {value!r}")
        return value


class UartHandle:
    def __init__(self, sim: Simulation, board: str, probe: str, from_ns: int | None = None) -> None:
        self._sim = sim
        self._board = board
        self._probe = probe
        self._from_ns = from_ns

    def since(self, duration: Duration) -> UartHandle:
        start = max(0, self._sim.virtual_time_ns - duration.nanoseconds)
        return UartHandle(self._sim, self._board, self._probe, from_ns=start)

    def readline(self) -> str | None:
        raw = self._sim._uart(self._board, self._probe).readline(self._from_ns)
        if raw is None:
            return None
        return raw.decode("utf-8", errors="replace")

    def read(self, count: int) -> bytes | None:
        return self._sim._uart(self._board, self._probe).read(count, self._from_ns)


class Simulation:
    def __init__(
        self,
        *,
        topology: Topology,
        csv: bool,
        testname: str,
        base_url: str | None = None,
    ) -> None:
        self.virtual_time_ns = 0
        self._csv = csv
        self._testname = safe_testname(testname)
        self._client = ClusterClient(base_url)
        self._viewer: ViewerHold | None = None
        self._id = self._client.create(topology.to_dict())
        self._viewer = ViewerHold(self._client.base_url, self._id)
        self._client.set_token(self._viewer.start())
        self._wait_catalog()
        self._uarts: dict[tuple[str, str], UartStream] = {}

    @property
    def id(self) -> str:
        return self._id

    def action(
        self,
        board_id: str,
        device: str,
        action: str,
        **params: Duration | int | float,
    ) -> None:
        converted: dict[str, float] = {}
        for name, value in params.items():
            if isinstance(value, Duration):
                converted[name] = float(value.nanoseconds)
            else:
                converted[name] = float(value)
        self._client.invoke_action(self._id, board_id, device, action, converted)

    def wait(self, duration: Duration) -> None:
        until_ns = self.virtual_time_ns + duration.nanoseconds
        self._client.start(self._id, until_ns)
        deadline = time.monotonic() + WAIT_TIMEOUT_S
        for status in self._client.iter_status(self._id):
            self.virtual_time_ns = int(status.get("virtual_time_ns") or 0)
            state = status.get("state")
            reason = status.get("stop_reason")
            if state == "exited":
                raise WaitError(f"simulation exited during wait (stop_reason={reason!r})")
            if state == "stopped":
                if self.virtual_time_ns >= until_ns or reason == "until":
                    return
                raise WaitError(f"wait stopped for {reason!r}")
            if time.monotonic() > deadline:
                raise WaitError("wait timed out before until_ns")
        raise WaitError("status stream ended before until_ns")

    def uart(self, board: str, probe: str) -> UartHandle:
        return UartHandle(self, board, probe)

    def digital(self, board: str, probe: str) -> DigitalHandle:
        return DigitalHandle(self, board, probe)

    def analog(self, board: str, probe: str) -> AnalogHandle:
        return AnalogHandle(self, board, probe)

    def close(self) -> None:
        csv_error: BaseException | None = None
        if self._csv:
            try:
                self._write_csv()
            except BaseException as exc:
                csv_error = exc
        try:
            self._client.delete(self._id)
        finally:
            if self._viewer is not None:
                self._viewer.close()
            self._client.close()
        if csv_error is not None:
            raise csv_error

    def _wait_catalog(self) -> None:
        deadline = time.monotonic() + CATALOG_TIMEOUT_S
        last: BaseException | None = None
        while time.monotonic() < deadline:
            try:
                self._client.catalog(self._id)
                return
            except Exception as exc:  # ApiError 503/409 while boards advertise
                last = exc
                if isinstance(exc, ApiError) and exc.status in {503, 409}:
                    time.sleep(0.2)
                    continue
                raise
        raise last if last is not None else WaitError("probe catalog is not ready")

    def _latest(self, board: str, probe: str, kind: str) -> dict[str, Any] | None:
        payload = self._client.samples(self._id, board, probe)
        self._raise_truncated(payload)
        samples = payload.get("samples") or []
        if not samples:
            return None
        sample = samples[-1]
        actual = sample.get("kind")
        if actual != kind:
            raise ProbeKindError(f"{board}/{probe} is {actual!r}, expected {kind!r}")
        return sample

    def _uart(self, board: str, probe: str) -> UartStream:
        key = (board, probe)
        stream = self._uarts.setdefault(key, UartStream())
        last = stream._words[-1] if stream._words else None
        last_vt = None if last is None else last.virtual_time_ns
        at_last = (
            sum(1 for word in stream._words if word.virtual_time_ns == last_vt)
            if last_vt is not None
            else 0
        )
        payload = self._client.samples(
            self._id,
            board,
            probe,
            from_ns=0 if last_vt is None else last_vt,
            to_ns=self.virtual_time_ns,
        )
        self._raise_truncated(payload)
        seen_at_last = 0
        for sample in payload.get("samples") or []:
            actual = sample.get("kind")
            if actual != "uart":
                raise ProbeKindError(f"{board}/{probe} is {actual!r}, expected 'uart'")
            vt = int(sample["virtual_time_ns"])
            value = sample["value"]
            if isinstance(value, bool) or not isinstance(value, int):
                raise ProbeKindError(f"{board}/{probe} uart value was {value!r}")
            if last_vt is not None and vt < last_vt:
                continue
            if last_vt is not None and vt == last_vt:
                seen_at_last += 1
                if seen_at_last <= at_last:
                    continue
            stream.push(vt, value)
        return stream

    def _raise_truncated(self, payload: dict[str, Any]) -> None:
        if payload.get("truncated"):
            raise TruncatedError("probe log was truncated")

    def _write_csv(self) -> None:
        stamp = datetime.now(timezone.utc).strftime("%Y%m%d%H%M%S")
        directory = out_dir() / self._testname
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / f"{self._testname}-{stamp}.csv"
        path.write_bytes(self._client.csv(self._id))


def open_simulation(
    topology_fn: Callable[[], Topology],
    *,
    csv: bool,
    testname: str,
    base_url: str | None = None,
) -> Simulation:
    topology = topology_fn()
    if not isinstance(topology, Topology):
        raise TypeError("topology function must return topology_dsl.Topology")
    return Simulation(
        topology=topology,
        csv=csv,
        testname=testname,
        base_url=base_url or default_base_url(),
    )
