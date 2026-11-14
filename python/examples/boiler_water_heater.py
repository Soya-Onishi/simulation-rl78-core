"""Water-heater cluster: piping-plant + boiler-plant + boiler_board.

Signal map (design lock):
  piping → boiler : inlet_temp_c, flow_lpm (scalar)
  boiler → piping : outlet_temp_c (scalar)
  boiler → board  : inlet_therm_v, outlet_therm_v (analog), flow_pulse (pulse)
  board  → boiler : uart0_tx → uart_heat_rx (uart)

Run / create (from repo root, with cluster up)::

  PYTHONPATH=python python -m topology_dsl run python/examples/boiler_water_heater.py
  PYTHONPATH=python python -m topology_dsl create python/examples/boiler_water_heater.py
"""

from __future__ import annotations

from pathlib import Path

from topology_dsl import SimulationConfig, Topology, simulation

_REPO_ROOT = Path(__file__).resolve().parents[2]
ELF_BOILER = str(_REPO_ROOT / "firmware" / "boiler" / "main.abs")


class BoilerWaterHeater(Topology):
    def __init__(self, name: str, *, elf_board: str) -> None:
        super().__init__(name)
        self._elf_board = elf_board

    def build(self) -> None:
        piping = self._plant("piping", kind="piping-plant")
        piping.endpoint("inlet_temp_c", direction="out", payload="scalar")
        piping.endpoint("flow_lpm", direction="out", payload="scalar")
        piping.endpoint("outlet_temp_c", direction="in", payload="scalar")

        boiler = self._plant("boiler", kind="boiler-plant")
        boiler.endpoint("inlet_temp_c", direction="in", payload="scalar")
        boiler.endpoint("flow_lpm", direction="in", payload="scalar")
        boiler.endpoint("outlet_temp_c", direction="out", payload="scalar")
        boiler.endpoint("inlet_therm_v", direction="out", payload="analog")
        boiler.endpoint("outlet_therm_v", direction="out", payload="analog")
        boiler.endpoint("flow_pulse", direction="out", payload="pulse")
        boiler.endpoint("uart_heat_rx", direction="in", payload="uart")

        board = self._board("board", kind="rl78boiler", elf=self._elf_board)
        board.endpoint("uart0_tx", direction="out", payload="uart")
        board.endpoint("adc_inlet", direction="in", payload="analog")
        board.endpoint("adc_outlet", direction="in", payload="analog")
        board.endpoint("flow_pulse", direction="in", payload="pulse")

        self._connect(piping.port("inlet_temp_c"), boiler.port("inlet_temp_c"))
        self._connect(piping.port("flow_lpm"), boiler.port("flow_lpm"))
        self._connect(boiler.port("outlet_temp_c"), piping.port("outlet_temp_c"))
        self._connect(boiler.port("inlet_therm_v"), board.port("adc_inlet"))
        self._connect(boiler.port("outlet_therm_v"), board.port("adc_outlet"))
        self._connect(boiler.port("flow_pulse"), board.port("flow_pulse"))
        self._connect(board.port("uart0_tx"), boiler.port("uart_heat_rx"))


@simulation
def boiler_water_heater() -> SimulationConfig:
    return SimulationConfig(
        margin_ns=1_000_000,
        topology=BoilerWaterHeater(name="boiler", elf_board=ELF_BOILER),
    )
