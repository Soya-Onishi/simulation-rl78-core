"""Development sample: requires a running cluster-server.

This is not a product system test. Real cases live in the development project.
``@sim_case`` skips automatically when the server is down.
"""

from __future__ import annotations

from systemtest import Duration, Topology, connect, sim_case, Simulation


def plant_topo() -> Topology:
    topology = Topology(margin_ns=200_000, headroom_threshold_ns=100_000)
    board = topology.board("a", kind="rl78")
    board.endpoint("dac0", direction="out", payload="analog")
    plant = topology.plant("p")
    plant.endpoint("vin", direction="in", payload="analog")
    plant.endpoint("vout", direction="out", payload="analog")
    connect(board.port("dac0"), plant.port("vin"))
    return topology


@sim_case(topology=plant_topo)
def test_plant_wait_and_analog_latest(sim: Simulation) -> None:
    sim.wait(Duration.ms(1))
    sim.analog("a", "dac0").latest()
    sim.analog("p", "vout").latest()
