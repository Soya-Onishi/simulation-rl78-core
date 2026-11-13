//! Water-heater plant — heat from UART, thermistors / flow pulse / outlet scalar.
//!
//! Hands control to [`sim_cluster::run_node`] via [`sim_cluster::node!`].
//! Physical step is 250 µs with first-order lag τ = 1 s on outlet temperature.
//! Heat uses the received `flow_lpm` as `Q` without capping; only the
//! `flow_pulse` output saturates at 25 L/min / 500 Hz.

use std::sync::{Arc, Mutex};

use sim_kernel::{
    AnalogVoltage, BoardPorts, InPort, Model, NS_PER_SEC, OutPort, Plant, ProbePoint, ProbePolicy,
    ProbeTap, Pulse, Scalar, SourcePort, Tick, UartFrame, Wire,
};

/// Solver step (250 µs).
const DT: Tick = Tick(250_000);
const TAU_NS: f64 = 1.0 * NS_PER_SEC as f64;
const MAX_HEAT_KCAL_PER_H: f64 = 15_000.0;
const MAX_FLOW_LPM: f64 = 25.0;
const MAX_PULSE_HZ: f64 = 500.0;
const FULL_SCALE_UV: f64 = 5_000_000.0;
const FULL_SCALE_TEMP_C: f64 = 100.0;
const THERM_PROBE_MIN_NS: u64 = 10_000_000;

sim_cluster::node!(build_plant);

struct BoilerState {
    tin_c: f64,
    flow_lpm: f64,
    heat_cmd: u8,
    tout_c: f64,
}

/// Water-heater physics and board-facing transducers.
pub struct BoilerModel {
    state: Arc<Mutex<BoilerState>>,
    outlet_src: SourcePort<Scalar>,
    inlet_therm_src: SourcePort<AnalogVoltage>,
    outlet_therm_src: SourcePort<AnalogVoltage>,
    pulse_src: SourcePort<Pulse>,
}

impl BoilerModel {
    fn attach(
        inlet_temp: &mut InPort<Scalar>,
        flow: &mut InPort<Scalar>,
        uart: &mut InPort<UartFrame>,
        outlet_temp: OutPort<Scalar>,
        inlet_therm: OutPort<AnalogVoltage>,
        outlet_therm: OutPort<AnalogVoltage>,
        flow_pulse: OutPort<Pulse>,
        inlet_tap: Arc<ProbeTap<AnalogVoltage>>,
        outlet_tap: Arc<ProbeTap<AnalogVoltage>>,
    ) -> Self {
        let state = Arc::new(Mutex::new(BoilerState {
            tin_c: 20.0,
            flow_lpm: 0.0,
            heat_cmd: 0,
            tout_c: 20.0,
        }));

        let tin = Arc::clone(&state);
        let _tin = Wire::new()
            .source(inlet_temp.port_mut())
            .sink(move |values, changed| {
                if let Some(&Scalar(v)) = values.get(changed) {
                    tin.lock().expect("boiler").tin_c = v;
                }
            });

        let flow_st = Arc::clone(&state);
        let _flow = Wire::new().source(flow.port_mut()).sink(move |values, changed| {
            if let Some(&Scalar(v)) = values.get(changed) {
                flow_st.lock().expect("boiler").flow_lpm = v.max(0.0);
            }
        });

        let heat = Arc::clone(&state);
        let _uart = Wire::new()
            .source(uart.port_mut())
            .sink(move |values, changed| {
                if let Some(frame) = values.get(changed) {
                    let cmd = (frame.data & 0xFF) as u8;
                    heat.lock().expect("boiler").heat_cmd = cmd.min(100);
                }
            });

        let mut outlet_src = SourcePort::new();
        let _outlet = Wire::new()
            .source(&mut outlet_src)
            .sink(move |values, changed| outlet_temp.on_input(values, changed));

        let mut inlet_therm_src = SourcePort::new();
        let _inlet_v = Wire::new()
            .source(&mut inlet_therm_src)
            .sink(move |values, changed| inlet_therm.on_input(values, changed))
            .sink(inlet_tap.callback());

        let mut outlet_therm_src = SourcePort::new();
        let _outlet_v = Wire::new()
            .source(&mut outlet_therm_src)
            .sink(move |values, changed| outlet_therm.on_input(values, changed))
            .sink(outlet_tap.callback());

        let mut pulse_src = SourcePort::new();
        let _pulse = Wire::new()
            .source(&mut pulse_src)
            .sink(move |values, changed| flow_pulse.on_input(values, changed));

        Self {
            state,
            outlet_src,
            inlet_therm_src,
            outlet_therm_src,
            pulse_src,
        }
    }

    #[cfg(test)]
    fn tout_c(&self) -> f64 {
        self.state.lock().expect("boiler").tout_c
    }

    #[cfg(test)]
    fn heat_cmd(&self) -> u8 {
        self.state.lock().expect("boiler").heat_cmd
    }
}

fn temp_to_uv(temp_c: f64) -> AnalogVoltage {
    let clamped = temp_c.clamp(0.0, FULL_SCALE_TEMP_C);
    let uv = (clamped / FULL_SCALE_TEMP_C * FULL_SCALE_UV).round() as i64;
    AnalogVoltage(uv)
}

fn pulse_for_flow(flow_lpm: f64) -> Pulse {
    if flow_lpm <= 0.0 {
        return Pulse::STOPPED;
    }
    let capped = flow_lpm.min(MAX_FLOW_LPM);
    let hz = (capped / MAX_FLOW_LPM) * MAX_PULSE_HZ;
    if hz <= 0.0 {
        return Pulse::STOPPED;
    }
    let half_period_ns = ((0.5 / hz) * NS_PER_SEC as f64).round() as u64;
    Pulse {
        hi_width_ns: half_period_ns.max(1),
        lo_width_ns: half_period_ns.max(1),
    }
}

impl Model for BoilerModel {
    fn dt(&self) -> Tick {
        DT
    }

    fn reset(&mut self) {
        {
            let mut g = self.state.lock().expect("boiler");
            g.tin_c = 20.0;
            g.flow_lpm = 0.0;
            g.heat_cmd = 0;
            g.tout_c = 20.0;
        }
        self.step();
    }

    fn step(&mut self) {
        let (tin, flow, tout) = {
            let mut g = self.state.lock().expect("boiler");
            let p = f64::from(g.heat_cmd) / 100.0 * MAX_HEAT_KCAL_PER_H;
            if g.flow_lpm > 0.0 {
                let tout_eq = g.tin_c + p / (60.0 * g.flow_lpm);
                let alpha = 1.0 - (-(DT.0 as f64) / TAU_NS).exp();
                g.tout_c += (tout_eq - g.tout_c) * alpha;
            }
            (g.tin_c, g.flow_lpm, g.tout_c)
        };
        self.outlet_src.drive(Scalar(tout));
        self.inlet_therm_src.drive(temp_to_uv(tin));
        self.outlet_therm_src.drive(temp_to_uv(tout));
        self.pulse_src.drive(pulse_for_flow(flow));
    }
}

fn build_plant() -> (Plant<BoilerModel>, BoardPorts, Vec<ProbePoint>) {
    let mut inlet_temp_c = InPort::new("inlet_temp_c").expect("inlet_temp_c");
    let mut flow_lpm = InPort::new("flow_lpm").expect("flow_lpm");
    let mut uart_heat_rx = InPort::new("uart_heat_rx").expect("uart_heat_rx");
    let outlet_temp_c = OutPort::new("outlet_temp_c").expect("outlet_temp_c");
    let inlet_therm_v = OutPort::new("inlet_therm_v").expect("inlet_therm_v");
    let outlet_therm_v = OutPort::new("outlet_therm_v").expect("outlet_therm_v");
    let flow_pulse = OutPort::new("flow_pulse").expect("flow_pulse");

    let inlet_tap = ProbeTap::new(
        "inlet_therm_v",
        ProbePolicy::MinInterval {
            min_ns: THERM_PROBE_MIN_NS,
        },
    );
    let outlet_tap = ProbeTap::new(
        "outlet_therm_v",
        ProbePolicy::MinInterval {
            min_ns: THERM_PROBE_MIN_NS,
        },
    );

    let model = BoilerModel::attach(
        &mut inlet_temp_c,
        &mut flow_lpm,
        &mut uart_heat_rx,
        outlet_temp_c.clone(),
        inlet_therm_v.clone(),
        outlet_therm_v.clone(),
        flow_pulse.clone(),
        Arc::clone(&inlet_tap),
        Arc::clone(&outlet_tap),
    );

    let mut ports = BoardPorts::new();
    ports.insert_in(inlet_temp_c).expect("inlet_temp_c");
    ports.insert_in(flow_lpm).expect("flow_lpm");
    ports.insert_in(uart_heat_rx).expect("uart_heat_rx");
    ports.insert_out(outlet_temp_c).expect("outlet_temp_c");
    ports.insert_out(inlet_therm_v).expect("inlet_therm_v");
    ports.insert_out(outlet_therm_v).expect("outlet_therm_v");
    ports.insert_out(flow_pulse).expect("flow_pulse");

    (
        Plant::new(model),
        ports,
        vec![
            ProbePoint::Analog(inlet_tap),
            ProbePoint::Analog(outlet_tap),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{UartParity, UartStopBits};

    #[test]
    fn zero_flow_freezes_outlet_temp() {
        let (mut plant, ports, _) = build_plant();
        ports
            .in_port::<Scalar>("inlet_temp_c")
            .unwrap()
            .receive(Scalar(20.0));
        ports
            .in_port::<Scalar>("flow_lpm")
            .unwrap()
            .receive(Scalar(0.0));
        ports
            .in_port::<UartFrame>("uart_heat_rx")
            .unwrap()
            .receive(UartFrame {
                data: 100,
                data_bits: 8,
                bit_time: Tick(100_000),
                stop_bits: UartStopBits::One,
                parity: UartParity::None,
                inverted: false,
            });
        assert_eq!(plant.model().heat_cmd(), 100);
        let before = plant.model().tout_c();
        for _ in 0..4_000 {
            plant.model_mut().step();
        }
        assert_eq!(plant.model().tout_c(), before);
    }

    #[test]
    fn heating_raises_outlet_toward_equilibrium() {
        let (mut plant, ports, _) = build_plant();
        ports
            .in_port::<Scalar>("inlet_temp_c")
            .unwrap()
            .receive(Scalar(20.0));
        ports
            .in_port::<Scalar>("flow_lpm")
            .unwrap()
            .receive(Scalar(10.0));
        ports
            .in_port::<UartFrame>("uart_heat_rx")
            .unwrap()
            .receive(UartFrame {
                data: 100,
                data_bits: 8,
                bit_time: Tick(100_000),
                stop_bits: UartStopBits::One,
                parity: UartParity::None,
                inverted: false,
            });
        // P=15000, Tout_eq = 20 + 15000/(60*10) = 45
        for _ in 0..4_000 {
            // 1 s of virtual time
            plant.model_mut().step();
        }
        let tout = plant.model().tout_c();
        let expected = 20.0 + (45.0 - 20.0) * (1.0 - (-1.0_f64).exp());
        assert!(
            (tout - expected).abs() < 0.05,
            "tout={tout} expected≈{expected}"
        );
    }

    #[test]
    fn pulse_saturates_at_25_lpm() {
        assert_eq!(
            pulse_for_flow(25.0),
            pulse_for_flow(40.0),
            "sensor pulse rate saturates at 25 L/min"
        );
        assert_eq!(pulse_for_flow(0.0), Pulse::STOPPED);
        let p = pulse_for_flow(25.0);
        assert_eq!(p.hi_width_ns, p.lo_width_ns);
        // 500 Hz → 1 ms period → 0.5 ms half
        assert_eq!(p.hi_width_ns, 1_000_000);
    }

    #[test]
    fn thermistor_maps_100c_to_5v() {
        assert_eq!(temp_to_uv(100.0), AnalogVoltage(5_000_000));
        assert_eq!(temp_to_uv(0.0), AnalogVoltage(0));
        assert_eq!(temp_to_uv(50.0), AnalogVoltage(2_500_000));
    }

    #[test]
    fn uart_heat_cmd_clips_above_100() {
        let (plant, ports, _) = build_plant();
        ports
            .in_port::<UartFrame>("uart_heat_rx")
            .unwrap()
            .receive(UartFrame {
                data: 200,
                data_bits: 8,
                bit_time: Tick(100_000),
                stop_bits: UartStopBits::One,
                parity: UartParity::None,
                inverted: false,
            });
        assert_eq!(plant.model().heat_cmd(), 100);
    }
}
