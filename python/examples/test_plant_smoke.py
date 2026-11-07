"""Development sample: requires a running cluster-backend and simulation-frontend.

This is not a product system test. Real cases live in the development project.
``@sim_case`` skips automatically when the server is down.
"""

from __future__ import annotations

from topology_dsl import SimulationConfig, Topology

from systemtest import Duration, Simulation, sim_case


class PlantTopo(Topology):
    def build(self) -> None:
        board = self.board("a", kind="rl78")
        board.endpoint("dac0", direction="out", payload="analog")
        plant = self.plant("p")
        plant.endpoint("vin", direction="in", payload="analog")
        plant.endpoint("vout", direction="out", payload="analog")
        self.connect(board.port("dac0"), plant.port("vin"))


def plant_config() -> SimulationConfig:
    return SimulationConfig(
        margin_ns=200_000,
        headroom_threshold_ns=100_000,
        topology=PlantTopo(name="demo"),
    )


@sim_case(config=plant_config)
def test_plant_wait_and_analog_latest(sim: Simulation) -> None:
    sim.wait(Duration.ms(1))
    sim.analog("demo/a", "dac0").latest()
    sim.analog("demo/p", "vout").latest()
