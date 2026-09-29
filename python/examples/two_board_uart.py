"""Example: two RL78 boards with bidirectional UART (issue #37 first slice)."""

from topology_dsl import Topology, connect, emit

t = Topology(margin_ns=200_000, headroom_threshold_ns=100_000)
a = t.board("a", kind="rl78", elf="https://github.com/Soya-Onishi/rl78-uart-sample/raw/refs/heads/main/uart-tx/main.abs")
b = t.board("b", kind="rl78", elf="https://github.com/Soya-Onishi/rl78-uart-sample/raw/refs/heads/main/uart-rx/main.abs")
a.endpoint("uart0_tx", direction="out", payload="uart")
a.endpoint("uart0_rx", direction="in", payload="uart")
b.endpoint("uart0_tx", direction="out", payload="uart")
b.endpoint("uart0_rx", direction="in", payload="uart")
connect(a.port("uart0_tx"), b.port("uart0_rx"))
emit(t)
