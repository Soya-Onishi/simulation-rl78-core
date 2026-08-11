//! Why the guest left the running state.

use crate::breakpoint::BreakpointId;
use crate::bus::Addr;

/// Stop reasons visible to CLI / future GDB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    Halt,
    ExternalStop,
    Breakpoint { id: BreakpointId },
    Unmapped { addr: Addr, write: bool },
}
