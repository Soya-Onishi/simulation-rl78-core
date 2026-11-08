//! Control-plane messages shared by arbiter, nodes, and iceoryx2 pub/sub.
//!
//! a2n ([`ControlToNode`]) and n2a ([`ControlToArbiter`]) are separate sample
//! types so destination vs sender hashing cannot be confused. Layout matches
//! the SHM sample (`repr(C)` + [`ZeroCopySend`]).

use std::fmt;

use iceoryx2::prelude::ZeroCopySend;

use crate::ipc::HashedId;

/// Host-initiated stop reasons on the control plane (no free-form strings).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum HostStopReason {
    Breakpoint = 1,
    ExternalStop = 2,
    Step = 3,
    Unmapped = 4,
    /// Virtual-time cap from host `start` was reached (or already passed).
    Until = 5,
}

impl HostStopReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Breakpoint => "breakpoint",
            Self::ExternalStop => "external_stop",
            Self::Step => "step",
            Self::Unmapped => "unmapped",
            Self::Until => "until",
        }
    }

    /// Parse wire / CLI labels (`breakpoint`, `external`, `external_stop`, …).
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "breakpoint" => Some(Self::Breakpoint),
            "external_stop" | "external" => Some(Self::ExternalStop),
            "step" => Some(Self::Step),
            "unmapped" => Some(Self::Unmapped),
            "until" => Some(Self::Until),
            _ => None,
        }
    }

    #[must_use]
    pub fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => None,
            1 => Some(Self::Breakpoint),
            2 => Some(Self::ExternalStop),
            3 => Some(Self::Step),
            4 => Some(Self::Unmapped),
            5 => Some(Self::Until),
            _ => None,
        }
    }

    #[must_use]
    pub fn to_wire(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for HostStopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Wire encoding of a node's control-plane state (n2a [`ControlToArbiter::State`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum NodeStateWire {
    Booting = 1,
    Stopped = 2,
    Running = 3,
}

/// Arbiter → node control messages (a2n iceoryx2 sample type).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
#[allow(clippy::large_enum_variant)]
pub enum ControlToNode {
    /// Request the node's current control-plane state (always broadcast).
    GetState,
    /// Simulation may enter Running (always broadcast).
    ///
    /// `allowed_ns` is the current run ceiling: `min(now + margin, until_ns)`
    /// when capped, otherwise `now + margin`. The node applies queued actions
    /// at the current virtual time, then may advance up to this value. Later
    /// [`Self::Allowed`] samples raise the ceiling as peers report.
    Start { allowed_ns: u64 },
    /// New virtual-time ceiling (always broadcast).
    Allowed { allowed_ns: u64 },
    /// Host-initiated cluster stop (always broadcast; no ack on same host).
    ClusterStop { reason: HostStopReason },
    /// Hold-reset devices and the CPU via [`sim_kernel::Resettable`].
    ///
    /// The arbiter sends this only after [`Self::ClusterStop`], then
    /// [`Self::Start`] (which carries the new ceiling).
    Reset,
    /// Replace the emit set for one board (always broadcast; nodes ignore others).
    ProbeWatch {
        board_hash: HashedId<u64>,
        count: u8,
        probe_ids: [HashedId<u64>; crate::probe::MAX_PROBES_PER_BOARD],
    },
    /// One instruction on `board_hash`; other nodes ignore this sample.
    GdbStep { board_hash: HashedId<u64> },
    // TODO: add Shutdown to end the node process (ClusterStop only halts to Stopped).
}

/// Node → arbiter control messages (n2a iceoryx2 sample type).
///
/// `from` is always the sender board hash (not a destination).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum ControlToArbiter {
    /// Reply to [`ControlToNode::GetState`].
    State {
        from: HashedId<u64>,
        state: NodeStateWire,
    },
    /// Virtual time report.
    TimeReport {
        from: HashedId<u64>,
        virtual_time_ns: u64,
    },
    /// Host-initiated stop (BP / external / step / unmapped).
    /// Guest-local Halt (sim stays Running) must not be sent.
    HostStop {
        from: HashedId<u64>,
        reason: HostStopReason,
    },
}

impl ControlToArbiter {
    /// Sender board hash.
    #[must_use]
    pub fn from_board(self) -> HashedId<u64> {
        match self {
            Self::State { from, .. }
            | Self::TimeReport { from, .. }
            | Self::HostStop { from, .. } => from,
        }
    }
}
