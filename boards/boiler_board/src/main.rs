//! Water-heater RL78 board — cluster binary (`kind=rl78boiler`).
//!
//! Assembles [`g23_machine`] and hands control to [`sim_cluster::run_node`] via
//! [`sim_cluster::node!`]. Board-edge ports match the water-heater topology;
//! plant physics and guest firmware live in later phases.

use rl78::{R7F100Gxl, Rl78G23Core};
use sim_kernel::{
    AnalogVoltage, BoardPorts, DigitalLevel, EventCtl, HasMemoryMap, HoldSwitch, InPort, Machine,
    OutPort, ProbePoint, ProbeTap, Pulse, PulseExpander, UnmappedPolicy, Wire,
};

sim_cluster::node!(g23_machine);

/// RL78/G23 water-heater board wiring.
///
/// Board-edge ports: `uart0_tx`, `adc_inlet`, `adc_outlet`, `flow_pulse`.
/// On-board devices: `temp_up` / `temp_down` ([`HoldSwitch`]).
///
/// Package map (Q39): P0.0 ← flow pulse, P0.1 ← `temp_up`, P0.2 ← `temp_down`,
/// ANI0 ← `adc_inlet`, ANI1 ← `adc_outlet`. UART0 TX is SAU-direct (no GPIO).
pub fn g23_machine() -> (Machine<R7F100Gxl>, BoardPorts, Vec<ProbePoint>) {
    let ctl = EventCtl::new();
    let part = R7F100Gxl::new(ctl.clone());
    let (mut ports, mut probes) = wire_board_uart0_tx(&part.core);
    wire_adc_inlets(&part, &mut ports, &mut probes);
    let flow_expander = wire_flow_pulse(&part, &ctl, &mut ports);
    let (temp_up, temp_down) = wire_temp_switches(&part);

    let bus = part
        .memory_map()
        .expect("g23 memory map")
        .policy(UnmappedPolicy::Log)
        .build()
        .unwrap();
    let machine = Machine::with_devices(
        part,
        bus,
        ctl,
        vec![
            Box::new(temp_up),
            Box::new(temp_down),
            Box::new(flow_expander),
        ],
    )
    .expect("devices");
    (machine, ports, probes)
}

/// Wire board-edge UART0 TX (`uart0_tx` → plant `uart_heat_rx`).
fn wire_board_uart0_tx(core: &Rl78G23Core) -> (BoardPorts, Vec<ProbePoint>) {
    let uart_tx = OutPort::new("uart0_tx").expect("uart0_tx");
    let tap = ProbeTap::new("uart0", sim_kernel::ProbePolicy::EveryDrive);
    {
        let mut sau = core.sau.lock().expect("sau");
        let tx = uart_tx.clone();
        let _tx = Wire::new()
            .source(sau.tx_source())
            .sink(move |values, changed| tx.on_input(values, changed))
            .sink(tap.callback());
    }
    let mut ports = BoardPorts::new();
    ports.insert_out(uart_tx).expect("uart0_tx");
    (ports, vec![ProbePoint::Uart(tap)])
}

/// `adc_inlet` / `adc_outlet` → ANI0 / ANI1, with Analog [`ProbePoint`]s.
fn wire_adc_inlets(part: &R7F100Gxl, ports: &mut BoardPorts, probes: &mut Vec<ProbePoint>) {
    for (name, channel) in [("adc_inlet", 0usize), ("adc_outlet", 1)] {
        let mut port = InPort::<AnalogVoltage>::new(name).expect(name);
        let pin = part.adc_input(channel);
        let tap = ProbeTap::new(name, sim_kernel::ProbePolicy::OnChange);
        let _wire = Wire::new()
            .source(port.port_mut())
            .sink(move |values, changed| {
                if let Some(&level) = values.get(changed) {
                    pin.drive(level);
                }
            })
            .sink(tap.callback());
        ports.insert_in(port).expect(name);
        probes.push(ProbePoint::Analog(tap));
    }
}

/// `flow_pulse` ([`Pulse`]) → [`PulseExpander`] → P0.0.
fn wire_flow_pulse(
    part: &R7F100Gxl,
    ctl: &EventCtl,
    ports: &mut BoardPorts,
) -> PulseExpander {
    let mut expander = PulseExpander::new("flow_pulse", ctl.clone()).expect("flow_pulse");
    let pin = part.gpio_input(0);
    let _gpio = Wire::new()
        .source(expander.level_source_mut())
        .sink(move |values, changed| {
            if let Some(&level) = values.get(changed) {
                pin.drive(level);
            }
        });
    let mut flow_in = InPort::<Pulse>::new("flow_pulse").expect("flow_pulse");
    let _rx = Wire::new()
        .source(flow_in.port_mut())
        .sink(expander.pulse_sink());
    ports.insert_in(flow_in).expect("flow_pulse");
    expander
}

/// `temp_up` → P0.1, `temp_down` → P0.2.
fn wire_temp_switches(part: &R7F100Gxl) -> (HoldSwitch, HoldSwitch) {
    let temp_up = wire_hold_switch("temp_up", part.gpio_input(1));
    let temp_down = wire_hold_switch("temp_down", part.gpio_input(2));
    (temp_up, temp_down)
}

fn wire_hold_switch(name: &str, pin: sim_kernel::SourcePort<DigitalLevel>) -> HoldSwitch {
    let mut sw = HoldSwitch::new(name).expect(name);
    let _wire = Wire::new()
        .source(sw.source_mut())
        .sink(move |values, changed| {
            if let Some(&level) = values.get(changed) {
                pin.drive(level);
            }
        });
    sw
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use sim_kernel::UartFrame;

    use super::*;

    #[serial]
    #[test]
    fn g23_machine_registers_temp_switches() {
        let (machine, _ports, _probes) = g23_machine();
        let catalog = machine.action_catalog();
        let names: Vec<_> = catalog
            .iter()
            .map(|e| (e.device.as_str(), e.action.as_str()))
            .collect();
        assert!(names.contains(&("temp_up", "hold")), "{names:?}");
        assert!(names.contains(&("temp_down", "hold")), "{names:?}");
        assert_eq!(catalog.len(), 2, "{catalog:?}");
    }

    #[serial]
    #[test]
    fn g23_machine_exposes_board_edge_ports() {
        let (_machine, ports, probes) = g23_machine();
        assert!(ports.out_port::<UartFrame>("uart0_tx").is_some());
        assert!(ports.in_port::<AnalogVoltage>("adc_inlet").is_some());
        assert!(ports.in_port::<AnalogVoltage>("adc_outlet").is_some());
        assert!(ports.in_port::<Pulse>("flow_pulse").is_some());
        assert!(
            probes
                .iter()
                .any(|p| matches!(p, ProbePoint::Uart(tap) if tap.name() == "uart0"))
        );
        assert!(
            probes
                .iter()
                .any(|p| matches!(p, ProbePoint::Analog(tap) if tap.name() == "adc_inlet"))
        );
        assert!(
            probes
                .iter()
                .any(|p| matches!(p, ProbePoint::Analog(tap) if tap.name() == "adc_outlet"))
        );
    }

    fn pump(machine: &mut Machine<R7F100Gxl>, now: sim_kernel::Tick) {
        let ctl = machine.event_ctl().clone();
        ctl.set_now(now);
        loop {
            let due = {
                let mut events = machine.events_mut();
                events.pop_due(now)
            };
            let Some((_, mut event)) = due else {
                break;
            };
            let mut ctx = sim_kernel::EventCtx {
                now,
                bus: machine.bus_mut(),
                stop: None,
            };
            event.fire(&mut ctx);
        }
    }

    #[serial]
    #[test]
    fn adc_inlet_receive_drives_ani0_conversion() {
        let (mut machine, ports, _probes) = g23_machine();
        machine.reset();
        ports.clear_pending();
        ports
            .in_port::<AnalogVoltage>("adc_inlet")
            .expect("adc_inlet")
            .receive(AnalogVoltage(2_500_000));

        machine.bus_mut().write(0xFFF32, &[0x20]).unwrap();
        machine.bus_mut().write(0xFFF30, &[0x80]).unwrap();
        pump(&mut machine, sim_kernel::Tick(1));
        pump(&mut machine, sim_kernel::Tick(72_000));

        let mut adcr = [0u8; 2];
        machine.bus_mut().read(0xFFF1E, &mut adcr).unwrap();
        assert_eq!(u16::from_le_bytes(adcr) >> 6, 0x0200);
    }

    #[serial]
    #[test]
    fn flow_pulse_receive_schedules_gpio_edges() {
        let (machine, ports, _probes) = g23_machine();
        ports
            .in_port::<Pulse>("flow_pulse")
            .expect("flow_pulse")
            .receive(Pulse {
                hi_width_ns: 1_000_000,
                lo_width_ns: 1_000_000,
            });
        assert!(
            machine.event_ctl().events().next_deadline().is_some(),
            "pulse expander should schedule at least one edge"
        );
    }
}
