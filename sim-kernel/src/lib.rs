//! Architecture-independent simulation kernel.
//!
//! Owns virtual time, the event queue, the memory bus contract, typed pin
//! wires, off-board [`InPort`] / [`OutPort`] endpoints, and the
//! start/stop/quit control surface. Architecture crates implement [`Core`] inside
//! a [`Cpu`] (on-chip part) and assemble a [`Machine`]; host front-ends (CLI, GDB)
//! talk through
//! [`Command`] / [`Response`] on the simulation thread. The optional GDB
//! stub lives in this crate and uses the same control plane.

mod action;
mod breakpoint;
mod bus;
mod clock;
mod command;
mod cpu;
pub mod elf;
mod entity;
mod event;
mod gdb;
mod machine;
mod plant;
mod ports;
mod probe;
mod reset;
mod sim;
mod stop;
mod switch;
mod virtual_time;
mod wiring;

#[cfg(test)]
mod testing;

pub use action::{
    Action, ActionCatalogEntry, ActionFailure, ActionSpec, Device, DeviceError, MAX_ACTION_PARAMS,
    ParamSpec, Parameters, id_hash, prepare_parameters,
};
pub use breakpoint::{Breakpoint, BreakpointId, BreakpointStore};
pub use bus::{
    Addr, BusError, HasMemoryMap, MapError, MemoryBus, MemoryMapBuilder, MemoryMapped, Ram, Rom,
    UnmappedAccess, UnmappedPolicy,
};
pub use clock::{NS_PER_SEC, Tick, VirtualClock};
pub use command::{Command, InspectResult, Response, SimError};
pub use cpu::{
    Core, Cpu, FirmwareError, PendingStop, Quantum, RegId, TlibExit, map_tlib_exit,
    resolve_after_tlib_execute,
};
pub use elf::{EM_RL78, ElfLoad, LoadError, load_elf, load_elf_into_machine};
pub use entity::{AdvanceCtx, Entity, EntityPoll};
pub use event::{EventCtl, EventCtx, EventId, EventQueue, SimEvent};
pub use gdb::{GdbBindError, listen_gdb, parse_gdb_dev};
pub use machine::Machine;
pub use plant::{Model, Plant};
pub use ports::{BoardPorts, InPort, OutPort, PortError, PortId};
pub use probe::{ProbePoint, ProbePolicy, ProbeSample, ProbeTap};
pub use reset::Resettable;
pub use sim::{
    BeforeQuantum, DEFAULT_MAX_QUANTUM, SimConfig, SimControl, SimEvents, SimState, Simulator,
    spawn,
};
pub use stop::StopReason;
pub use switch::{HoldSwitch, MAX_HOLD_MS};
pub use virtual_time::{set_global_virtual_time_ns, virtual_time_ns};
pub use wiring::{
    AnalogVoltage, DigitalLevel, SolidWire, SourcePort, U0, U1, UartFrame, UartParity,
    UartStopBits, Wire,
};

#[cfg(test)]
mod sim_tests;
