//! CPU contract implemented by architecture crates.

use crate::breakpoint::{Breakpoint, BreakpointId};
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

/// Latch for stops requested from *callbacks* during execute (MMIO trap helpers,
/// watchpoint hooks, etc.).
///
/// Software breakpoints from `tlib_add_breakpoint` are **not** the primary use:
/// those make `tlib_execute` return `EXCP_DEBUG` (see [`map_tlib_exit`]). Use this
/// latch when a host callback must force a stop that is not already expressed as
/// a tlib exception index.
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

/// Subset of tlib exit codes returned by `tlib_execute` / `cpu_exec`
/// (`include/cpu-defs.h`).
///
/// Breakpoint hits from `tlib_add_breakpoint` exit as [`Self::Debug`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum TlibExit {
    /// `EXCP_INTERRUPT` — quantum / external interrupt exit, not a guest stop.
    Interrupt = 0x10000,
    /// `EXCP_WFI` — wait-for-interrupt / idle.
    Wfi = 0x10001,
    /// `EXCP_DEBUG` — stopped after a breakpoint or single-step.
    Debug = 0x10002,
    /// `EXCP_WATCHPOINT`.
    Watchpoint = 0x10004,
    /// `EXCP_RETURN_REQUEST`.
    ReturnRequest = 0x10005,
}

impl TlibExit {
    #[must_use]
    pub fn from_raw(code: i32) -> Option<Self> {
        match code {
            x if x == Self::Interrupt as i32 => Some(Self::Interrupt),
            x if x == Self::Wfi as i32 => Some(Self::Wfi),
            x if x == Self::Debug as i32 => Some(Self::Debug),
            x if x == Self::Watchpoint as i32 => Some(Self::Watchpoint),
            x if x == Self::ReturnRequest as i32 => Some(Self::ReturnRequest),
            _ => None,
        }
    }
}

/// Map a `tlib_execute` return value into a [`StopReason`].
///
/// This is where `Quantum.stop = StopReason::Breakpoint` is decided for tlib:
/// `cpu_exec` returns `exception_index`, and a GDB breakpoint sets
/// `EXCP_DEBUG` (`0x10002`). The architecture crate resolves `id` from PC /
/// the breakpoint table and passes it as `breakpoint_at_pc`.
#[must_use]
pub fn map_tlib_exit(exit: i32, breakpoint_at_pc: Option<BreakpointId>) -> Option<StopReason> {
    match TlibExit::from_raw(exit) {
        Some(TlibExit::Debug) => Some(StopReason::Breakpoint {
            id: breakpoint_at_pc.unwrap_or(BreakpointId(0)),
        }),
        Some(TlibExit::Wfi) => Some(StopReason::Halt),
        Some(TlibExit::Interrupt)
        | Some(TlibExit::ReturnRequest)
        | Some(TlibExit::Watchpoint)
        | None => None,
    }
}

/// Architecture CPU. All mutation happens on the simulation thread.
pub trait Cpu: Send {
    /// Install the guest memory map used by load/store callbacks.
    ///
    /// Called **once** when the CPU is placed into a [`crate::Machine`] (bus
    /// layout is fixed after construction). Not invoked every quantum and not
    /// an argument to `tlib_execute`.
    fn bind_memory(&mut self, _bus: &mut MemoryBus) {}

    /// Execute up to `max_instructions` guest instructions.
    ///
    /// Mirrors `tlib_execute(max_insns)`. On return, map the exit code with
    /// [`map_tlib_exit`]: breakpoint → `EXCP_DEBUG` → [`StopReason::Breakpoint`].
    /// Implementations must either retire a non-zero number of instructions or
    /// return a stop reason. A zero-instruction quantum with no stop is treated
    /// as [`StopReason::Halt`].
    fn run_quantum(&mut self, max_instructions: u64) -> Quantum;

    fn read_reg(&self, id: RegId) -> Result<u64, SimError>;
    fn write_reg(&mut self, id: RegId, value: u64) -> Result<(), SimError>;

    fn pc(&self) -> Addr;
    fn set_pc(&mut self, pc: Addr);

    /// Push the kernel breakpoint table into the backend (`tlib_add_breakpoint`).
    fn sync_breakpoints(&mut self, _breakpoints: &[Breakpoint]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excp_debug_maps_to_breakpoint() {
        let stop = map_tlib_exit(TlibExit::Debug as i32, Some(BreakpointId(7)));
        assert_eq!(
            stop,
            Some(StopReason::Breakpoint {
                id: BreakpointId(7)
            })
        );
    }

    #[test]
    fn excp_interrupt_is_not_a_stop() {
        assert_eq!(map_tlib_exit(TlibExit::Interrupt as i32, None), None);
    }
}
