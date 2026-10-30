"""pytest plugin: @sim_case and the sim fixture."""

from __future__ import annotations

from collections.abc import Iterator

import pytest

from systemtest.client import cluster_available
from systemtest.simulation import Simulation, open_simulation


def pytest_configure(config: pytest.Config) -> None:
    config.addinivalue_line("markers", "sim_case(topology, csv=False): cluster simulation case")


def pytest_runtest_setup(item: pytest.Item) -> None:
    """Skip @sim_case tests when cluster-server is not listening."""
    if item.get_closest_marker("sim_case") is None:
        return
    if not cluster_available():
        pytest.skip("cluster-server is not running")


def sim_case(*, topology, csv: bool = False):
    """Mark a test that receives a connected Simulation as argument ``sim``.

    If cluster-server is down, the test is skipped automatically.
    """

    def decorator(fn):
        return pytest.mark.sim_case(topology=topology, csv=csv)(fn)

    return decorator


@pytest.fixture
def sim(request: pytest.FixtureRequest) -> Iterator[Simulation]:
    marker = request.node.get_closest_marker("sim_case")
    if marker is None:
        pytest.fail("sim fixture requires @sim_case(topology=...)")
    topology = marker.kwargs["topology"]
    csv = bool(marker.kwargs.get("csv", False))
    session = open_simulation(topology, csv=csv, testname=request.node.name)
    try:
        yield session
    finally:
        session.close()
