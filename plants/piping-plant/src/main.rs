//! Piping plant — supply temperature / pressure / faucet → flow and inlet temp.
//!
//! Hands control to [`sim_cluster::run_node`] via [`sim_cluster::node!`].
//! Host actions: `pressure`, `water_temp_c`, `faucet` (each with `set`).
//! `outlet_temp_c` is latched for the faucet return path; it does not feed the
//! algebraic flow / inlet-temperature outputs.

use std::sync::{Arc, Mutex};

use sim_kernel::{
    BoardPorts, InPort, Knob, Model, OutPort, Plant, ProbePoint, Scalar, SourcePort, Tick, Wire,
};

/// Solver step (1 ms). Piping outputs are algebraic in the actions.
const DT: Tick = Tick(1_000_000);
const FLOW_AT_BASE_PRESSURE_LPM: f64 = 25.0;

sim_cluster::node!(build_plant);

struct PipingState {
    outlet_temp_c: f64,
}

/// Algebraic piping model driven by host knobs.
pub struct PipingModel {
    pressure: Arc<Mutex<f64>>,
    water_temp_c: Arc<Mutex<f64>>,
    faucet: Arc<Mutex<f64>>,
    state: Arc<Mutex<PipingState>>,
    inlet_src: SourcePort<Scalar>,
    flow_src: SourcePort<Scalar>,
}

impl PipingModel {
    fn attach(
        pressure: Arc<Mutex<f64>>,
        water_temp_c: Arc<Mutex<f64>>,
        faucet: Arc<Mutex<f64>>,
        outlet: &mut InPort<Scalar>,
        inlet_out: OutPort<Scalar>,
        flow_out: OutPort<Scalar>,
    ) -> Self {
        let state = Arc::new(Mutex::new(PipingState {
            outlet_temp_c: 20.0,
        }));
        let state_in = Arc::clone(&state);
        let _outlet = Wire::new()
            .source(outlet.port_mut())
            .sink(move |values, changed| {
                if let Some(&Scalar(v)) = values.get(changed) {
                    state_in.lock().expect("piping").outlet_temp_c = v;
                }
            });

        let mut inlet_src = SourcePort::new();
        let _inlet = Wire::new()
            .source(&mut inlet_src)
            .sink(move |values, changed| inlet_out.on_input(values, changed));

        let mut flow_src = SourcePort::new();
        let _flow = Wire::new()
            .source(&mut flow_src)
            .sink(move |values, changed| flow_out.on_input(values, changed));

        Self {
            pressure,
            water_temp_c,
            faucet,
            state,
            inlet_src,
            flow_src,
        }
    }

    #[cfg(test)]
    fn outlet_temp_c(&self) -> f64 {
        self.state.lock().expect("piping").outlet_temp_c
    }
}

impl Model for PipingModel {
    fn dt(&self) -> Tick {
        DT
    }

    fn reset(&mut self) {
        {
            let mut g = self.state.lock().expect("piping");
            g.outlet_temp_c = *self.water_temp_c.lock().expect("water");
        }
        self.step();
    }

    fn step(&mut self) {
        let pressure = *self.pressure.lock().expect("pressure");
        let water_temp_c = *self.water_temp_c.lock().expect("water");
        let faucet = *self.faucet.lock().expect("faucet");
        let flow_lpm = faucet * pressure * FLOW_AT_BASE_PRESSURE_LPM;
        self.inlet_src.drive(Scalar(water_temp_c));
        self.flow_src.drive(Scalar(flow_lpm));
    }
}

fn build_plant() -> (Plant<PipingModel>, BoardPorts, Vec<ProbePoint>) {
    let pressure = Knob::new(
        "pressure",
        1.0,
        0.0,
        2.0,
        Some("norm"),
        "配管の相対水圧（1.0=基準）",
    )
    .expect("pressure");
    let water_temp_c = Knob::new(
        "water_temp_c",
        20.0,
        0.0,
        100.0,
        Some("C"),
        "配管端の水温",
    )
    .expect("water_temp_c");
    let faucet = Knob::new("faucet", 0.0, 0.0, 1.0, Some("norm"), "カラン開度").expect("faucet");

    let mut outlet_temp_c = InPort::new("outlet_temp_c").expect("outlet_temp_c");
    let inlet_temp_c = OutPort::new("inlet_temp_c").expect("inlet_temp_c");
    let flow_lpm = OutPort::new("flow_lpm").expect("flow_lpm");

    let model = PipingModel::attach(
        pressure.value_handle(),
        water_temp_c.value_handle(),
        faucet.value_handle(),
        &mut outlet_temp_c,
        inlet_temp_c.clone(),
        flow_lpm.clone(),
    );

    let mut ports = BoardPorts::new();
    ports.insert_in(outlet_temp_c).expect("outlet_temp_c");
    ports.insert_out(inlet_temp_c).expect("inlet_temp_c");
    ports.insert_out(flow_lpm).expect("flow_lpm");

    let plant = Plant::with_devices(
        model,
        vec![Box::new(pressure), Box::new(water_temp_c), Box::new(faucet)],
    )
    .expect("devices");

    (plant, ports, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{ActionFailure, Entity, id_hash};

    #[test]
    fn flow_is_faucet_times_pressure_times_25() {
        let (mut plant, ports, _) = build_plant();
        plant
            .apply_action(
                id_hash("faucet"),
                id_hash("set"),
                Tick::ZERO,
                &[(id_hash("value"), 1.0)],
            )
            .unwrap();
        plant
            .apply_action(
                id_hash("pressure"),
                id_hash("set"),
                Tick::ZERO,
                &[(id_hash("value"), 1.0)],
            )
            .unwrap();
        plant.model_mut().step();
        let flow = ports
            .out_port::<Scalar>("flow_lpm")
            .unwrap()
            .pending()
            .last()
            .copied();
        assert_eq!(flow, Some(Scalar(25.0)));
        let inlet = ports
            .out_port::<Scalar>("inlet_temp_c")
            .unwrap()
            .pending()
            .last()
            .copied();
        assert_eq!(inlet, Some(Scalar(20.0)));
    }

    #[test]
    fn pressure_out_of_range_is_rejected() {
        let (mut plant, _, _) = build_plant();
        let err = plant
            .apply_action(
                id_hash("pressure"),
                id_hash("set"),
                Tick::ZERO,
                &[(id_hash("value"), 2.5)],
            )
            .unwrap_err();
        assert_eq!(err, ActionFailure::BadParam);
    }

    #[test]
    fn outlet_temp_is_latched() {
        let (plant, ports, _) = build_plant();
        ports
            .in_port::<Scalar>("outlet_temp_c")
            .unwrap()
            .receive(Scalar(42.0));
        assert_eq!(plant.model().outlet_temp_c(), 42.0);
    }

    #[test]
    fn action_catalog_lists_three_knobs() {
        let (plant, _, _) = build_plant();
        let catalog = plant.action_catalog();
        assert_eq!(catalog.len(), 3);
        let devices: Vec<_> = catalog.iter().map(|e| e.device.as_str()).collect();
        assert!(devices.contains(&"pressure"));
        assert!(devices.contains(&"water_temp_c"));
        assert!(devices.contains(&"faucet"));
    }

    #[test]
    fn reset_restores_knob_defaults_and_outputs() {
        let (mut plant, ports, _) = build_plant();
        plant
            .apply_action(
                id_hash("faucet"),
                id_hash("set"),
                Tick::ZERO,
                &[(id_hash("value"), 1.0)],
            )
            .unwrap();
        plant
            .apply_action(
                id_hash("water_temp_c"),
                id_hash("set"),
                Tick::ZERO,
                &[(id_hash("value"), 55.0)],
            )
            .unwrap();
        plant.model_mut().step();
        plant.reset();
        plant.model_mut().step();
        let flow = ports
            .out_port::<Scalar>("flow_lpm")
            .unwrap()
            .pending()
            .last()
            .copied();
        let inlet = ports
            .out_port::<Scalar>("inlet_temp_c")
            .unwrap()
            .pending()
            .last()
            .copied();
        assert_eq!(flow, Some(Scalar(0.0)));
        assert_eq!(inlet, Some(Scalar(20.0)));
    }
}
