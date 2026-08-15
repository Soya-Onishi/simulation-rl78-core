//! Architecture-independent simulation kernel.
//!
//! Owns virtual time, the event queue, the memory bus contract, typed pin
//! wires, and the
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
mod reset;
mod sim;
mod stop;
mod wiring;

#[cfg(test)]
mod testing;

pub use breakpoint::{Breakpoint, BreakpointId, BreakpointStore};
pub use bus::{
    Addr, BusError, HasMemoryMap, MapError, MemoryBus, MemoryMapBuilder, MemoryMapped, Ram, Rom,
    UnmappedAccess, UnmappedPolicy,
};
pub use clock::{NS_PER_SEC, Tick, VirtualClock};
pub use command::{Command, InspectResult, Response, SimError};
pub use cpu::{
    Cpu, PendingStop, Quantum, RegId, TlibExit, map_tlib_exit, resolve_after_tlib_execute,
};
pub use event::{EventCtl, EventCtx, EventId, EventQueue, SimEvent};
pub use machine::Machine;
pub use reset::Resettable;
pub use sim::{SimConfig, SimControl, SimEvents, SimState, Simulator, spawn};
pub use stop::StopReason;
pub use wiring::{AnalogVoltage, DigitalLevel, SolidWire, SourcePort, U0, U1, Wire};

#[cfg(test)]
mod sim_tests;
