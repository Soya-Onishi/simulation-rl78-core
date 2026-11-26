//! Node process runtime (#141 / #147 / #153).
//!
//! [`NodeRuntime`] runs the IPC / Probe / Simulation threads plus a log thread
//! that forwards their `log` records toward the Orchestrator host (a dummy
//! stream until the TCP link exists). Phase 2 wires the control plane only:
//! Start / Stop / Reset over the Command ↔ Node RR service and the `host_ctrl`
//! emergency stop. Guest / plant execution lands in Phase 3.

mod ipc;
mod logger;
mod probe;
mod runtime;
mod simulation;

pub use runtime::{NodeRuntime, NodeRuntimeError};
pub use simulation::SimState;
