//! CPU contract implemented by architecture crates.

use crate::breakpoint::Breakpoint;
use crate::bus::{Addr, MemoryBus};
use crate::command::SimError;
use crate::stop::StopReason;

/// Architecture-defined register identifier (stable numeric id, not a string).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegId(pub u32);

/// Result of one quantum of guest execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quantum {
    /// Guest instructions retired in this quantum.
    pub instructions: u64,
    /// Guest-originated stop, if any (`Halt`, breakpoint, unmapped, …).
    pub stop: Option<StopReason>,
}

/// Latch for stops requested from tlib/memory/debug callbacks during execute.
///
/// Backends set this from a callback (then request tlib to return), and fold it
/// into [`Quantum::stop`] when [`Cpu::run_quantum`] completes. The kernel never
/// scans PC for breakpoints itself.
#[derive(Clone, Debug, Default)]
pub struct PendingStop {
    reason: Option<StopReason>,
}

impl PendingStop {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request(&mut self, reason: StopReason) {
        if self.reason.is_none() {
            self.reason = Some(reason);
        }
    }

    pub fn take(&mut self) -> Option<StopReason> {
        self.reason.take()
    }
}

/// Architecture CPU. All mutation happens on the simulation thread.
pub trait Cpu: Send {
    /// Bind guest memory for callbacks used during the next [`run_quantum`].
    ///
    /// `tlib_execute` has no memory-map argument; MMIO / softmmu callbacks reach
    /// [`MemoryBus`] through state installed here (pointer, thread-local, …).
    fn bind_memory(&mut self, _bus: &mut MemoryBus) {}

    /// Execute up to `max_instructions` guest instructions.
    ///
    /// Mirrors `tlib_execute(max_insns)`: no bus argument. Implementations must
    /// either retire a non-zero number of instructions or return a stop reason.
    /// A zero-instruction quantum with no stop is treated as [`StopReason::Halt`].
    ///
    /// Breakpoints: sync via [`sync_breakpoints`] (`tlib_add_breakpoint`). On hit,
    /// tlib returns / a CB requests exit; map that to [`Quantum::stop`] =
    /// [`StopReason::Breakpoint`] (see [`PendingStop`]). Do not rely on the kernel
    /// to compare PC after the quantum.
    fn run_quantum(&mut self, max_instructions: u64) -> Quantum;

    fn read_reg(&self, id: RegId) -> Result<u64, SimError>;
    fn write_reg(&mut self, id: RegId, value: u64) -> Result<(), SimError>;

    fn pc(&self) -> Addr;
    fn set_pc(&mut self, pc: Addr);

    /// Push the kernel breakpoint table into the backend (e.g. `tlib_add_breakpoint`).
    fn sync_breakpoints(&mut self, _breakpoints: &[Breakpoint]) {}
}
