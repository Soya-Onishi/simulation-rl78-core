//! Architecture-independent simulation kernel.
//!
//! Owns virtual time, the event queue, the memory bus contract, and the
//! start/stop/quit control surface. Architecture crates implement [`Cpu`] and
//! assemble a [`Machine`]; host front-ends (CLI now, GDB later) talk only
//! through [`Command`] / [`Response`] on the simulation thread.

mod breakpoint;
mod bus;
mod clock;
mod command;
mod cpu;
mod event;
mod machine;
mod sim;
mod stop;
mod testing;

pub use breakpoint::{Breakpoint, BreakpointId, BreakpointStore};
pub use bus::{
    Addr, BusError, MapError, MemoryBus, MemoryMapped, Ram, Rom, UnmappedAccess, UnmappedPolicy,
};
pub use clock::{Tick, VirtualClock};
pub use command::{Command, InspectResult, Response, SimError};
pub use cpu::{Cpu, Quantum, RegId};
pub use event::{EventCtx, EventId, EventQueue, SimEvent};
pub use machine::Machine;
pub use sim::{SimConfig, SimControl, SimEvents, SimState, Simulator, spawn};
pub use stop::StopReason;

pub use testing::ScriptedCpu;

#[cfg(test)]
mod sim_tests;
