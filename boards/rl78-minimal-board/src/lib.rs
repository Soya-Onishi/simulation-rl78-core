//! RL78 minimal board: G23 part plus board-edge UART0 and SW1.

use std::sync::Arc;

use rl78::{R7F100Gxl, Rl78G23Core, SauUnit};
use sim_kernel::{
    BoardPorts, EventCtl, HasMemoryMap, HoldSwitch, InPort, Machine, OutPort, ProbePoint, ProbeTap,
    UnmappedPolicy, Wire,
};

/// Knobs for [`g23_machine`]. Option byte `0x000C2` is read from ROM at reset.
#[derive(Clone, Debug, Default)]
pub struct G23MachineConfig {
    pub unmapped: UnmappedPolicy,
}

/// Wire board-edge UART0 (`uart0_tx` / `uart0_rx`).
///
/// Endpoint names match the cluster topology; this is board glue, not SoC wiring.
fn wire_board_uart0(core: &Rl78G23Core) -> (BoardPorts, Vec<ProbePoint>) {
    let uart_tx = OutPort::new("uart0_tx").expect("uart0_tx");
    let mut uart_rx = InPort::new("uart0_rx").expect("uart0_rx");

    let tap = ProbeTap::new("uart0", sim_kernel::ProbePolicy::EveryDrive);

    {
        let mut sau = core.sau.lock().expect("sau");
        let tx = uart_tx.clone();
        let _tx = Wire::new()
            .source(sau.tx_source())
            .sink(move |values, changed| tx.on_input(values, changed))
            .sink(tap.callback());
    }
    {
        let sau = Arc::clone(&core.sau);
        let _rx = Wire::new()
            .source(uart_rx.port_mut())
            .sink(move |values, changed| SauUnit::on_rx(&sau, 1, values, changed));
    }
    let mut ports = BoardPorts::new();
    ports.insert_out(uart_tx).expect("uart0_tx");
    ports.insert_in(uart_rx).expect("uart0_rx");
    (ports, vec![ProbePoint::Uart(tap)])
}

/// RL78/G23 + R7F100GxL RAM/ROM. Option byte is applied at core reset from ROM.
///
/// Returns the machine, board-edge [`BoardPorts`] (`uart0_tx` / `uart0_rx`), and
/// wiring-side [`ProbePoint`]s attached while the wires were built.
///
/// `sw1` owns its [`sim_kernel::SourcePort`] and is wired to package pin P0.0.
/// Package pins are not board [`InPort`]s or [`OutPort`]s.
pub fn g23_machine(cfg: G23MachineConfig) -> (Machine<R7F100Gxl>, BoardPorts, Vec<ProbePoint>) {
    let ctl = EventCtl::new();
    let part = R7F100Gxl::new(ctl.clone());
    let (ports, probes) = wire_board_uart0(&part.core);
    let bus = part
        .memory_map()
        .expect("g23 memory map")
        .policy(cfg.unmapped)
        .build()
        .unwrap();
    let mut sw1 = HoldSwitch::new("sw1").expect("sw1");
    let pin = part.gpio_input(0);
    let _sw = Wire::new()
        .source(sw1.source_mut())
        .sink(move |values, changed| {
            if let Some(&level) = values.get(changed) {
                pin.drive(level);
            }
        });
    let machine = Machine::with_devices(part, bus, ctl, vec![Box::new(sw1)]).expect("sw1");
    (machine, ports, probes)
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    #[serial]
    #[test]
    fn g23_machine_registers_sw1_hold() {
        let (machine, _ports, _probes) = g23_machine(G23MachineConfig::default());
        let catalog = machine.action_catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].device, "sw1");
        assert_eq!(catalog[0].action, "hold");
        assert_eq!(catalog[0].device_hash, sim_kernel::id_hash("sw1"));
        assert_eq!(catalog[0].action_hash, sim_kernel::id_hash("hold"));
    }
}
