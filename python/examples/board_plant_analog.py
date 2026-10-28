"""Example: RL78 board analog DAC into a first-order-lag plant."""

from topology_dsl import Topology, connect, emit

t = Topology(margin_ns=200_000, headroom_threshold_ns=100_000)
a = t.board("a", kind="rl78")
a.endpoint("dac0", direction="out", payload="analog")
p = t.plant("p")
p.endpoint("vin", direction="in", payload="analog")
p.endpoint("vout", direction="out", payload="analog")
connect(a.port("dac0"), p.port("vin"))
emit(t)
