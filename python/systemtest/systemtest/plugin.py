"""pytest plugin: @sim_case and the sim fixture."""

from __future__ import annotations

from collections.abc import Iterator

import pytest

from systemtest.client import cluster_available
from systemtest.simulation import Simulation, open_simulation


def pytest_configure(config: pytest.Config) -> None:
    config.addinivalue_line("markers", "sim_case(root, csv=False): cluster simulation case")


def pytest_runtest_setup(item: pytest.Item) -> None:
    """Skip @sim_case tests when simulation-frontend is not listening."""
    if item.get_closest_marker("sim_case") is None:
        return
    if not cluster_available():
        pytest.skip("simulation-frontend is not running")


def sim_case(*, root, csv: bool = False):
    """Mark a test that receives a connected Simulation as argument ``sim``.

    ``root`` must be a ``topology_dsl.Root`` instance. The ``sim`` fixture calls
    ``root.emit()`` when the test starts.

    If simulation-frontend is down, the test is skipped automatically.
    """

    def decorator(fn):
        return pytest.mark.sim_case(root=root, csv=csv)(fn)

    return decorator


@pytest.fixture
def sim(request: pytest.FixtureRequest) -> Iterator[Simulation]:
    marker = request.node.get_closest_marker("sim_case")
    if marker is None:
        pytest.fail("sim fixture requires @sim_case(root=...)")
    root = marker.kwargs["root"]
    csv = bool(marker.kwargs.get("csv", False))
    session = open_simulation(root, csv=csv, testname=request.node.name)
    try:
        yield session
    finally:
        session.close()
