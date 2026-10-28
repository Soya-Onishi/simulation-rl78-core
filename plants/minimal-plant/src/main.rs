//! Minimal plant — first-order lag on analog `vin` / `vout`.
//!
//! Hands control to [`sim_cluster::run_node`]. No firmware; [`sim_kernel::Model::dt`]
//! is 1 ms and the time constant is 5 s.

use std::env;
use std::process;
use std::sync::{Arc, Mutex};

use sim_cluster::{BoardCliError, board_usage, parse_board_args, run_node};
use sim_kernel::{
    AnalogVoltage, BoardPorts, InPort, Model, NS_PER_SEC, OutPort, Plant, ProbePoint, ProbePolicy,
    ProbeTap, SourcePort, Tick, Wire,
};

/// Solver step (1 ms).
const DT: Tick = Tick(1_000_000);
/// Time constant T = 5 s, in nanoseconds.
const TAU_NS: f64 = 5.0 * NS_PER_SEC as f64;

fn main() {
    let mut argv = env::args();
    let prog = argv.next().unwrap_or_else(|| "minimal-plant".into());
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

    if let Err(err) = run_node(opts, build_plant) {
        eprintln!("minimal-plant: {err}");
        process::exit(1);
    }
}

/// First-order lag `y' = (u - y) / T` with forward Euler.
pub struct FirstOrderLag {
    inner: Arc<Mutex<LagInner>>,
    y_src: SourcePort<AnalogVoltage>,
}

struct LagInner {
    u_uv: i64,
    y_uv: i64,
}

impl FirstOrderLag {
    fn attach(
        vin: &mut InPort<AnalogVoltage>,
        vout: OutPort<AnalogVoltage>,
        vin_tap: Arc<ProbeTap<AnalogVoltage>>,
        vout_tap: Arc<ProbeTap<AnalogVoltage>>,
    ) -> Self {
        let inner = Arc::new(Mutex::new(LagInner { u_uv: 0, y_uv: 0 }));
        let inner_in = Arc::clone(&inner);
        let _vin = Wire::new()
            .source(vin.port_mut())
            .sink(move |values, changed| {
                if let Some(&AnalogVoltage(uv)) = values.get(changed) {
                    inner_in.lock().expect("lag").u_uv = uv;
                }
            })
            .sink(vin_tap.callback());
        let mut y_src = SourcePort::new();
        let _vout = Wire::new()
            .source(&mut y_src)
            .sink(move |values, changed| vout.on_input(values, changed))
            .sink(vout_tap.callback());
        Self { inner, y_src }
    }
}

#[cfg(test)]
impl FirstOrderLag {
    fn u_uv(&self) -> i64 {
        self.inner.lock().expect("lag").u_uv
    }

    fn y_uv(&self) -> i64 {
        self.inner.lock().expect("lag").y_uv
    }
}

impl Model for FirstOrderLag {
    fn dt(&self) -> Tick {
        DT
    }

    fn reset(&mut self) {
        {
            let mut g = self.inner.lock().expect("lag");
            g.u_uv = 0;
            g.y_uv = 0;
        }
        self.y_src.drive(AnalogVoltage(0));
    }

    fn step(&mut self) {
        let y_uv = {
            let mut g = self.inner.lock().expect("lag");
            let alpha = DT.0 as f64 / TAU_NS;
            let y = g.y_uv as f64 + alpha * (g.u_uv as f64 - g.y_uv as f64);
            g.y_uv = y.round() as i64;
            g.y_uv
        };
        self.y_src.drive(AnalogVoltage(y_uv));
    }
}

fn build_plant() -> (Plant<FirstOrderLag>, BoardPorts, Vec<ProbePoint>) {
    let mut vin = InPort::new("vin").expect("vin");
    let vout = OutPort::new("vout").expect("vout");
    let vin_tap = ProbeTap::new("vin", ProbePolicy::OnChange);
    let vout_tap = ProbeTap::new("vout", ProbePolicy::OnChange);
    let model = FirstOrderLag::attach(&mut vin, vout.clone(), vin_tap.clone(), vout_tap.clone());
    let mut ports = BoardPorts::new();
    ports.insert_in(vin).expect("vin");
    ports.insert_out(vout).expect("vout");
    (
        Plant::new(model),
        ports,
        vec![ProbePoint::Analog(vin_tap), ProbePoint::Analog(vout_tap)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{Command, Entity, SimConfig, Simulator};

    fn wired_lag() -> (FirstOrderLag, InPort<AnalogVoltage>, OutPort<AnalogVoltage>) {
        let mut vin = InPort::new("vin").unwrap();
        let vout = OutPort::new("vout").unwrap();
        let vin_tap = ProbeTap::new("vin", ProbePolicy::OnChange);
        let vout_tap = ProbeTap::new("vout", ProbePolicy::OnChange);
        let model = FirstOrderLag::attach(&mut vin, vout.clone(), vin_tap, vout_tap);
        (model, vin, vout)
    }

    #[test]
    fn step_zero_to_five_volts_approaches_input() {
        let (mut model, vin, vout) = wired_lag();
        vin.receive(AnalogVoltage(5_000_000));
        assert_eq!(model.u_uv(), 5_000_000);
        model.step();
        let first = model.y_uv();
        assert!(first > 0, "first step should leave 0 V");
        assert!(first < 5_000_000);
        for _ in 0..5_000 {
            model.step();
        }
        let y = model.y_uv();
        // 1 τ (5 s): 5 V * (1 - e^{-1}) ≈ 3.16 V
        let expected = (5_000_000.0 * (1.0 - (-1.0_f64).exp())).round() as i64;
        assert!((y - expected).abs() < 5_000, "y={y} expected≈{expected}");
        assert_eq!(vout.pending().last().copied(), Some(AnalogVoltage(y)));
    }

    #[test]
    fn plant_reset_clears_state_and_keeps_clock() {
        let (model, vin, _vout) = wired_lag();
        vin.receive(AnalogVoltage(5_000_000));
        let mut sim = Simulator::new(Plant::new(model), SimConfig::default());
        sim.command(Command::SetAllowed {
            tick: Tick(10_000_000),
        });
        sim.command(Command::Start);
        assert_eq!(sim.poll(), None);
        sim.command(Command::Stop);
        // Idle-advance to dt so the clock has moved, then reset.
        sim.command(Command::Start);
        assert_eq!(sim.poll(), None);
        sim.command(Command::Stop);
        let now = sim.entity().now();
        sim.entity_mut().reset();
        assert_eq!(sim.entity().now(), now);
        assert_eq!(sim.entity().model().u_uv(), 0);
        assert_eq!(sim.entity().model().y_uv(), 0);
        assert_eq!(sim.entity().next_step_at(), now);
    }
}
