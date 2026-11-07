"""Example: RL78 board analog DAC into a first-order-lag plant."""

from topology_dsl import Root, Topology


class BoardPlantAnalog(Topology):
    def build(self) -> None:
        a = self.board("a", kind="rl78")
        a.endpoint("dac0", direction="out", payload="analog")
        p = self.plant("p")
        p.endpoint("vin", direction="in", payload="analog")
        p.endpoint("vout", direction="out", payload="analog")
        self.connect(a.port("dac0"), p.port("vin"))


class BoardPlantAnalogRoot(Root):
    def configure(self) -> None:
        self.margin_ns = 200_000
        self.headroom_threshold_ns = 100_000
        self.topology = BoardPlantAnalog(name="demo")


BoardPlantAnalogRoot().emit()
