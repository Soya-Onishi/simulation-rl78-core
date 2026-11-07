"""Example: two RL78 boards with bidirectional UART (issue #37 first slice)."""

from topology_dsl import Root, Topology

ELF_TX = "https://github.com/Soya-Onishi/rl78-uart-sample/raw/refs/heads/main/uart-tx/main.abs"
ELF_RX = "https://github.com/Soya-Onishi/rl78-uart-sample/raw/refs/heads/main/uart-rx/main.abs"


class TwoBoardUart(Topology):
    def __init__(self, name: str, *, elf_a: str, elf_b: str) -> None:
        super().__init__(name)
        self._elf_a = elf_a
        self._elf_b = elf_b

    def build(self) -> None:
        a = self.board("a", kind="rl78", elf=self._elf_a)
        b = self.board("b", kind="rl78", elf=self._elf_b)
        a.endpoint("uart0_tx", direction="out", payload="uart")
        a.endpoint("uart0_rx", direction="in", payload="uart")
        b.endpoint("uart0_tx", direction="out", payload="uart")
        b.endpoint("uart0_rx", direction="in", payload="uart")
        self.connect(a.port("uart0_tx"), b.port("uart0_rx"))
        self.connect(b.port("uart0_tx"), a.port("uart0_rx"))


class TwoBoardUartRoot(Root):
    def configure(self) -> None:
        self.margin_ns = 200_000
        self.headroom_threshold_ns = 100_000
        self.topology = TwoBoardUart(name="demo", elf_a=ELF_TX, elf_b=ELF_RX)


TwoBoardUartRoot().emit()
