//! Why a quantum or control action produced a [`StopReason`].
//!
//! Only some reasons clear [`crate::SimState::Running`] (external stop, step,
//! breakpoint, unmapped). [`StopReason::Halt`] is guest-local idle: the sim
//! stays Running and does not emit [`crate::Response::Stopped`], so later polls
//! can wake on IRQs / events without confusing CLI/GDB.

use crate::breakpoint::BreakpointId;
use crate::bus::Addr;

/// Stop reasons visible to CLI / future GDB.
///
/// Architecture backends (tlib) report guest-originated stops through
/// [`crate::Quantum::stop`], including breakpoints discovered via tlib debug
/// callbacks — not via a kernel-side PC scan after each quantum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Guest-local idle (e.g. RL78 STOP / tlib WFI). Keeps the sim Running and
    /// is not surfaced as [`crate::Response::Stopped`]; must not freeze peers.
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
