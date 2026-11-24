//! Node process runtime (#141 / #147 / #153).
//!
//! [`NodeRuntime`] runs the IPC / Probe / Simulation threads. Phase 2 wires the
//! control plane only: Start / Stop / Reset over the Command ↔ Node RR service
//! and the `host_ctrl` emergency stop. Guest / plant execution lands in Phase 3.

mod probe;
mod runtime;
mod simulation;

pub use runtime::{NodeConfig, NodeRuntime, NodeRuntimeError};
pub use simulation::SimState;
