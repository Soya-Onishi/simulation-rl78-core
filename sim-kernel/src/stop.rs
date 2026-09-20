//! Why a quantum or control action produced a [`StopReason`].
//!
//! Only some reasons clear [`crate::SimState::Running`] (external stop, step,
//! breakpoint, unmapped). [`StopReason::Halt`] is guest-local idle and keeps
//! the sim Running so later polls can wake on IRQs / events.

use crate::breakpoint::BreakpointId;
use crate::bus::Addr;

/// Stop reasons visible to CLI / future GDB.
///
/// Architecture backends (tlib) report guest-originated stops through
/// [`crate::Quantum::stop`], including breakpoints discovered via tlib debug
/// callbacks — not via a kernel-side PC scan after each quantum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Guest-local idle (e.g. RL78 STOP / tlib WFI). Keeps the sim Running;
    /// must not freeze peer boards.
    Halt,
    ExternalStop,
    /// Finished a [`crate::Command::Step`] without another stop reason.
    Step,
    Breakpoint {
        id: BreakpointId,
    },
    Unmapped {
        addr: Addr,
        write: bool,
    },
}
