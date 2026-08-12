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
    /// `bus` is the guest physical map for this quantum: tlib memory callbacks
    /// (and mock CPUs) perform loads/stores through it while instructions run.
    /// The kernel does not walk the bus itself during execute.
    ///
    /// Implementations must either consume a non-zero number of ticks or return
    /// a stop reason. A zero-tick quantum with no stop is treated as [`StopReason::Halt`].
    /// Breakpoints are reported via [`Quantum::stop`] (`StopReason::Breakpoint`);
    /// the kernel does not re-scan the breakpoint table after each quantum.
    fn run_quantum(&mut self, bus: &mut MemoryBus, max_ticks: Tick) -> Quantum;

    fn read_reg(&self, id: RegId) -> Result<u64, SimError>;
    fn write_reg(&mut self, id: RegId, value: u64) -> Result<(), SimError>;

    fn pc(&self) -> Addr;
    fn set_pc(&mut self, pc: Addr);

    /// Push the kernel breakpoint table into the backend (e.g. `tlib_add_breakpoint`).
    fn sync_breakpoints(&mut self, _breakpoints: &[Breakpoint]) {}
}
