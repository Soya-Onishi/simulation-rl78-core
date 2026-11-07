"""Cluster system-test harness."""

from __future__ import annotations

from typing import Any

from topology_dsl import Board, Root, Topology

from systemtest.duration import Duration
from systemtest.errors import (
    ApiError,
    ProbeKindError,
    SystemTestError,
    TruncatedError,
    WaitError,
)
from systemtest.simulation import Simulation

__all__ = [
    "ApiError",
    "Board",
    "Duration",
    "ProbeKindError",
    "Root",
    "Simulation",
    "SystemTestError",
    "Topology",
    "TruncatedError",
    "WaitError",
    "sim_case",
]


def __getattr__(name: str) -> Any:
    # Lazy: importing plugin pulls pytest, which unit tests that only need
    # Simulation should not require.
    if name == "sim_case":
        from systemtest.plugin import sim_case

        return sim_case
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
