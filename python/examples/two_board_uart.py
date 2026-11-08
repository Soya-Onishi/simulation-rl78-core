"""Example: two RL78 boards with bidirectional UART (issue #37 first slice)."""

from topology_dsl import SimulationConfig, Topology, simulation

ELF_TX = "https://github.com/Soya-Onishi/rl78-uart-sample/raw/refs/heads/main/uart-tx/main.abs"
ELF_RX = "https://github.com/Soya-Onishi/rl78-uart-sample/raw/refs/heads/main/uart-rx/main.abs"


class TwoBoardUart(Topology):
    def __init__(self, name: str, *, elf_a: str, elf_b: str) -> None:
        super().__init__(name)
        self._elf_a = elf_a
        self._elf_b = elf_b

    def build(self) -> None:
        a = self._board("a", kind="rl78", elf=self._elf_a)
        b = self._board("b", kind="rl78", elf=self._elf_b)
        a.endpoint("uart0_tx", direction="out", payload="uart")
        a.endpoint("uart0_rx", direction="in", payload="uart")
        b.endpoint("uart0_tx", direction="out", payload="uart")
        b.endpoint("uart0_rx", direction="in", payload="uart")
        self._connect(a.port("uart0_tx"), b.port("uart0_rx"))
        self._connect(b.port("uart0_tx"), a.port("uart0_rx"))


@simulation
def two_board_uart() -> SimulationConfig:
    return SimulationConfig(
        margin_ns=200_000,
        topology=TwoBoardUart(name="demo", elf_a=ELF_TX, elf_b=ELF_RX),
    )
