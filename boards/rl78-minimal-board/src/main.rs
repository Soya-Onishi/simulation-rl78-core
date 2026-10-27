//! RL78 minimal board — cluster board binary with a fixed ROM/RAM map.
//!
//! Assembles [`g23_machine`] and hands control to [`sim_cluster::run_board`].
//! Firmware loading (if the topology lists an ELF) is performed by the
//! framework via [`sim_kernel::Machine::load_firmware`].

use std::env;
use std::process;

use sim_cluster::{BoardCliError, board_usage, parse_board_args, run_board};

use std::sync::Arc;

use dac::LevelDac;
use rl78::{R7F100Gxl, Rl78G23Core, SauUnit};
use sim_kernel::{
    BoardPorts, EventCtl, HasMemoryMap, HoldSwitch, InPort, Machine, OutPort, ProbePoint, ProbeTap,
    UnmappedPolicy, Wire,
};

fn main() {
    let mut argv = env::args();
    let prog = argv.next().unwrap_or_else(|| "rl78-minimal-board".into());
    let opts = match parse_board_args(&prog, argv) {
        Ok(opts) => opts,
        Err(BoardCliError::Help) => {
            eprint!("{}", board_usage(&prog));
            process::exit(0);
        }
        Err(BoardCliError::Message(msg)) => {
            eprintln!("{msg}");
            eprint!("{}", board_usage(&prog));
            process::exit(2);
        }
    };

    if let Err(err) = run_board(opts, g23_machine) {
        eprintln!("rl78-minimal-board: {err}");
        process::exit(1);
    }
}

/// RL78/G23 + R7F100GxL RAM/ROM. Option byte is applied at core reset from ROM.
///
/// Returns the machine, board-edge [`BoardPorts`] (`uart0_tx` / `uart0_rx` /
/// `dac0`), and wiring-side [`ProbePoint`]s attached while the wires were built.
///
/// `sw1` owns its [`sim_kernel::SourcePort`] and is wired to package pin P0.0.
/// Package pin P0.1 drives [`LevelDac`]; the board [`OutPort`] `dac0` is bound
/// here, like UART0 TX. Package pins are not board [`InPort`]s or [`OutPort`]s.
pub fn g23_machine() -> (Machine<R7F100Gxl>, BoardPorts, Vec<ProbePoint>) {
    let ctl = EventCtl::new();
    let part = R7F100Gxl::new(ctl.clone());
    let (mut ports, mut probes) = wire_board_uart0(&part.core);

    let mut dac = LevelDac::new("dac0").expect("dac0");
    let dac0 = OutPort::new("dac0").expect("dac0");
    let tap = ProbeTap::new("dac0", sim_kernel::ProbePolicy::OnChange);
    {
        let analog = dac0.clone();
        let _dac_tx = Wire::new()
            .source(dac.source_mut())
            .sink(move |values, changed| analog.on_input(values, changed))
            .sink(tap.callback());
    }
    {
        let gpio = part.gpio_output(1);
        let mut src = gpio.lock().expect("p0.1");
        let _dac_in = Wire::new().source(&mut src).sink(dac.callback());
    }
    ports.insert_out(dac0).expect("dac0");
    probes.push(ProbePoint::Analog(tap));

    let bus = part
        .memory_map()
        .expect("g23 memory map")
        .policy(UnmappedPolicy::Log)
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
    let machine =
        Machine::with_devices(part, bus, ctl, vec![Box::new(sw1), Box::new(dac)]).expect("devices");
    (machine, ports, probes)
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

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    #[serial]
    #[test]
    fn g23_machine_registers_sw1_hold() {
        let (machine, _ports, _probes) = g23_machine();
        let catalog = machine.action_catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].device, "sw1");
        assert_eq!(catalog[0].action, "hold");
        assert_eq!(catalog[0].device_hash, sim_kernel::id_hash("sw1"));
        assert_eq!(catalog[0].action_hash, sim_kernel::id_hash("hold"));
    }

    #[serial]
    #[test]
    fn p01_level_dac_high_low_hiz() {
        let (machine, ports, probes) = g23_machine();
        assert!(
            probes
                .iter()
                .any(|p| matches!(p, ProbePoint::Analog(tap) if tap.name() == "dac0"))
        );
        let dac0 = ports.out_port::<sim_kernel::AnalogVoltage>("dac0").unwrap();
        machine
            .cpu()
            .core
            .gpio
            .lock()
            .expect("gpio")
            .out_source(1)
            .drive(sim_kernel::DigitalLevel::High);
        assert_eq!(
            dac0.pending().last().copied(),
            Some(sim_kernel::AnalogVoltage(dac::HIGH_UV))
        );
        dac0.clear_pending();
        machine
            .cpu()
            .core
            .gpio
            .lock()
            .expect("gpio")
            .out_source(1)
            .drive(sim_kernel::DigitalLevel::Low);
        assert_eq!(
            dac0.pending().last().copied(),
            Some(sim_kernel::AnalogVoltage(0))
        );
        dac0.clear_pending();
        machine
            .cpu()
            .core
            .gpio
            .lock()
            .expect("gpio")
            .out_source(1)
            .drive(sim_kernel::DigitalLevel::High);
        dac0.clear_pending();
        machine
            .cpu()
            .core
            .gpio
            .lock()
            .expect("gpio")
            .out_source(1)
            .drive(sim_kernel::DigitalLevel::HiZ);
        assert_eq!(
            dac0.pending().last().copied(),
            Some(sim_kernel::AnalogVoltage(dac::HIGH_UV))
        );
    }
}
