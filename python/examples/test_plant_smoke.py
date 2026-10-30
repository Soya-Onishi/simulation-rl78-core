"""Development sample: requires a running cluster-server.

This is not a product system test. Real cases live in the development project.
"""

from __future__ import annotations

import os

import httpx
import pytest
from systemtest import Duration, Topology, connect, sim_case

DEFAULT_URL = os.environ.get("SYSTEMTEST_URL", "http://127.0.0.1:8090")


def _server_up() -> bool:
    try:
        httpx.get(DEFAULT_URL, timeout=0.5)
        return True
    except (httpx.HTTPError, httpx.RequestError):
        return False


def plant_topo() -> Topology:
    topology = Topology(margin_ns=200_000, headroom_threshold_ns=100_000)
    board = topology.board("a", kind="rl78")
    board.endpoint("dac0", direction="out", payload="analog")
    plant = topology.plant("p")
    plant.endpoint("vin", direction="in", payload="analog")
    plant.endpoint("vout", direction="out", payload="analog")
    connect(board.port("dac0"), plant.port("vin"))
    return topology


@pytest.mark.skipif(not _server_up(), reason="cluster-server is not running")
@sim_case(topology=plant_topo)
def test_plant_wait_and_analog_latest(sim) -> None:
    sim.wait(Duration.ms(1))
    sim.analog("a", "dac0").latest()
    sim.analog("p", "vout").latest()
