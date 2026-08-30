//! Control-plane messages. Front-ends only send commands and display responses.

use std::fmt;

use crate::breakpoint::BreakpointId;
use crate::bus::{Addr, BusError};
use crate::cpu::RegId;
use crate::stop::StopReason;

/// Commands accepted by the simulation thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Start,
    /// Run exactly one instruction (or until a nested stop), then stop.
    Step,
    Stop,
    Quit,
    ReadReg {
        id: RegId,
    },
    WriteReg {
        id: RegId,
        value: u64,
    },
    ReadMem {
        addr: Addr,
        len: u32,
    },
    WriteMem {
        addr: Addr,
        data: Vec<u8>,
    },
    AddBreakpoint {
        addr: Addr,
    },
    RemoveBreakpoint {
        id: BreakpointId,
    },
    /// Placeholder for a future multi-board arbiter. Currently a no-op.
    ///
    /// TODO: Invoke this from the kernel when a stop is *committed* (not from
    /// the GDB stub). Only debugger / cluster-relevant reasons should notify
    /// other board processes — e.g. [`StopReason::Breakpoint`],
    /// [`StopReason::ExternalStop`], [`StopReason::Step`]. Guest-local halts
    /// such as RL78 `STOP` / WFI ([`StopReason::Halt`]) must **not** freeze
    /// peer simulations.
    NotifyHalt {
        reason: StopReason,
    },
}

/// Successful inspect / mutation payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectResult {
    Ok,
    Reg { id: RegId, value: u64 },
    Mem { addr: Addr, data: Vec<u8> },
    Breakpoint { id: BreakpointId },
}

/// Recoverable control-plane error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SimError {
    /// Inspect / mutate was requested while the guest is running.
    Running,
    UnknownRegister(RegId),
    UnknownBreakpoint(BreakpointId),
    Bus(BusError),
}

/// Notifications and command completions produced by the simulation thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Started,
    Stopped(StopReason),
    Quit,
    Inspect(InspectResult),
    Error(SimError),
}

impl fmt::Display for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Started => write!(f, "started"),
            Self::Stopped(reason) => write!(f, "stopped: {reason}"),
            Self::Quit => write!(f, "quit"),
            Self::Inspect(result) => write!(f, "{result}"),
            Self::Error(err) => write!(f, "error: {err}"),
        }
    }
}

impl fmt::Display for InspectResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => write!(f, "ok"),
            Self::Reg { id, value } => write!(f, "reg {} = {value:#x}", id.0),
            Self::Mem { addr, data } => {
                write!(f, "mem {addr:#x} =")?;
                for byte in data {
                    write!(f, " {byte:02x}")?;
                }
                Ok(())
            }
            Self::Breakpoint { id } => write!(f, "breakpoint {}", id.0),
        }
    }
}

impl fmt::Display for SimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => write!(f, "simulation is running"),
            Self::UnknownRegister(id) => write!(f, "unknown register {}", id.0),
            Self::UnknownBreakpoint(id) => write!(f, "unknown breakpoint {}", id.0),
            Self::Bus(err) => write!(f, "{err}"),
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StopReason::Halt => write!(f, "halt"),
            StopReason::ExternalStop => write!(f, "external"),
            StopReason::Step => write!(f, "step"),
            StopReason::Breakpoint { id } => write!(f, "breakpoint {}", id.0),
            StopReason::Unmapped { addr, write } => {
                let kind = if *write { "write" } else { "read" };
                write!(f, "unmapped {kind} @ {addr:#x}")
            }
        }
    }
}
