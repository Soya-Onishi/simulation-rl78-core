//! Command → Node iceoryx Request-Response shapes (#141).
//!
//! Fixed-capacity `repr(C)` + [`ZeroCopySend`] samples (same pattern as
//! `sim-cluster` action IPC). REST / JSON uses
//! [`crate::orchestrator2command::NodeCommandDto`].

pub mod rr;

use iceoryx2::prelude::ZeroCopySend;
use sim_kernel::MAX_ACTION_PARAMS;

use crate::id::HashedId;

/// Max opaque inspect request / result bytes on the RR sample.
pub const INSPECT_CAP: usize = 1024;

/// Max probe ids in one subscribe command (matches node probe advertisement cap).
pub const MAX_NODE_PROBE_IDS: usize = 32;

/// Requests handled by the Node IPC thread (Simulation applies most; Probe
/// thread handles [`NodeCommand::ProbeSubscribe`]).
#[allow(clippy::large_enum_variant)]
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ZeroCopySend)]
pub enum NodeCommand {
    Start,
    Stop,
    Reset,
    GdbStep,
    /// Advance simulation virtual time up to `until_ns` (no-op if already past).
    GdbStepSync {
        until_ns: u64,
    },
    Inspect {
        len: u16,
        payload: [u8; INSPECT_CAP],
    },
    Action {
        device: HashedId<u64>,
        action: HashedId<u64>,
        param_count: u8,
        params: [f64; MAX_ACTION_PARAMS],
    },
    /// Add probe ids to the Node Probe thread subscription set.
    ProbeSubscribe {
        count: u8,
        probe_ids: [HashedId<u64>; MAX_NODE_PROBE_IDS],
    },
}

impl NodeCommand {
    /// Build an inspect command; returns `None` if `bytes` exceed [`INSPECT_CAP`].
    #[must_use]
    pub fn inspect(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > INSPECT_CAP {
            return None;
        }
        let mut payload = [0u8; INSPECT_CAP];
        payload[..bytes.len()].copy_from_slice(bytes);
        Some(Self::Inspect {
            len: bytes.len() as u16,
            payload,
        })
    }

    /// Build a probe subscribe command; returns `None` if too many ids.
    #[must_use]
    pub fn probe_subscribe(ids: &[HashedId<u64>]) -> Option<Self> {
        if ids.len() > MAX_NODE_PROBE_IDS {
            return None;
        }
        let mut probe_ids = [HashedId::ZERO; MAX_NODE_PROBE_IDS];
        probe_ids[..ids.len()].copy_from_slice(ids);
        Some(Self::ProbeSubscribe {
            count: ids.len() as u8,
            probe_ids,
        })
    }

    /// True when length / count fields are within the fixed caps.
    #[must_use]
    pub fn counts_valid(self) -> bool {
        match self {
            Self::Inspect { len, .. } => usize::from(len) <= INSPECT_CAP,
            Self::Action { param_count, .. } => usize::from(param_count) <= MAX_ACTION_PARAMS,
            Self::ProbeSubscribe { count, .. } => usize::from(count) <= MAX_NODE_PROBE_IDS,
            _ => true,
        }
    }
}

/// Success / error replies on the RR path.
#[allow(clippy::large_enum_variant)]
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ZeroCopySend)]
pub enum NodeCommandResult {
    Success,
    InspectResult {
        len: u16,
        payload: [u8; INSPECT_CAP],
    },
    Error(NodeCommandError),
}

/// Result error codes on the RR sample (`repr(u8)` for a stable wire value).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeCommandError {
    Unavailable = 1,
    ImplFault = 2,
    NotFoundDevice = 3,
    NotFoundAction = 4,
    BadParam = 5,
    Busy = 6,
}

// SAFETY: field-less `repr(u8)` enum; same pattern as `sim_kernel::DigitalLevel`.
// A corrupt discriminant from an untrusted peer is undefined for match; this
// path is same-host Command↔Node only.
unsafe impl ZeroCopySend for NodeCommandError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_rejects_oversize() {
        let big = vec![0u8; INSPECT_CAP + 1];
        assert!(NodeCommand::inspect(&big).is_none());
        assert!(NodeCommand::inspect(&[1, 2, 3]).is_some());
    }

    #[test]
    fn probe_subscribe_caps() {
        let ids: Vec<_> = (0..MAX_NODE_PROBE_IDS as u64).map(HashedId::new).collect();
        assert!(NodeCommand::probe_subscribe(&ids).is_some());
        let too_many: Vec<_> = (0..=MAX_NODE_PROBE_IDS as u64).map(HashedId::new).collect();
        assert!(NodeCommand::probe_subscribe(&too_many).is_none());
    }
}
