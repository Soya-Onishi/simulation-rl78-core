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

#[cfg(test)]
mod testing;

pub use breakpoint::{Breakpoint, BreakpointId, BreakpointStore};
pub use bus::{
    Addr, BusError, MapError, MemoryBus, MemoryMapBuilder, MemoryMapped, Ram, Rom, UnmappedAccess,
    UnmappedPolicy,
};
pub use clock::{Tick, VirtualClock};
pub use command::{Command, InspectResult, Response, SimError};
pub use cpu::{Cpu, PendingStop, Quantum, RegId, TlibExit, map_tlib_exit};
pub use event::{EventCtx, EventId, EventQueue, SimEvent};
pub use machine::Machine;
pub use sim::{SimConfig, SimControl, SimEvents, SimState, Simulator, spawn};
pub use stop::StopReason;

#[cfg(test)]
mod sim_tests;
