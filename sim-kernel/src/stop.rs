//! Why the guest left the running state.

use crate::breakpoint::BreakpointId;
use crate::bus::Addr;

/// Stop reasons visible to CLI / future GDB.
///
/// Architecture backends (tlib) report guest-originated stops through
/// [`crate::Quantum::stop`], including breakpoints discovered via tlib debug
/// callbacks — not via a kernel-side PC scan after each quantum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
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
