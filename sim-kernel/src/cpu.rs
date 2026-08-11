//! CPU contract implemented by architecture crates.

use crate::breakpoint::Breakpoint;
use crate::bus::{Addr, MemoryBus};
use crate::clock::Tick;
use crate::command::SimError;
use crate::stop::StopReason;

/// Architecture-defined register identifier (stable numeric id, not a string).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegId(pub u32);

/// Result of one quantum of guest execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quantum {
    /// Virtual time consumed by this quantum.
    pub ticks: Tick,
    /// Guest-originated stop, if any (`Halt`, breakpoint, unmapped, …).
    pub stop: Option<StopReason>,
}

/// Architecture CPU. All mutation happens on the simulation thread.
pub trait Cpu: Send {
    /// Execute up to `max_ticks` of guest work (M1: one instruction per tick).
    ///
    /// Implementations must either consume a non-zero number of ticks or return
    /// a stop reason. A zero-tick quantum with no stop is treated as [`StopReason::Halt`].
    fn run_quantum(&mut self, bus: &mut MemoryBus, max_ticks: Tick) -> Quantum;

    fn read_reg(&self, id: RegId) -> Result<u64, SimError>;
    fn write_reg(&mut self, id: RegId, value: u64) -> Result<(), SimError>;

    fn pc(&self) -> Addr;
    fn set_pc(&mut self, pc: Addr);

    /// Optional hook so backends can push the kernel breakpoint table into tlib.
    fn sync_breakpoints(&mut self, _breakpoints: &[Breakpoint]) {}
}
