"""Example: RL78 board analog DAC into a first-order-lag plant."""

from topology_dsl import SimulationConfig, Topology, simulation


class BoardPlantAnalog(Topology):
    def build(self) -> None:
        a = self._board("a", kind="rl78")
        a.endpoint("dac0", direction="out", payload="analog")
        p = self._plant("p")
        p.endpoint("vin", direction="in", payload="analog")
        p.endpoint("vout", direction="out", payload="analog")
        self._connect(a.port("dac0"), p.port("vin"))


@simulation
def board_plant_analog() -> SimulationConfig:
    return SimulationConfig(
        margin_ns=200_000,
        headroom_threshold_ns=100_000,
        topology=BoardPlantAnalog(name="demo"),
    )
