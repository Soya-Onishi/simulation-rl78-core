//! Control-plane messages shared by arbiter, nodes, and iceoryx2 pub/sub.
//!
//! a2n ([`ControlToNode`]) and n2a ([`ControlToArbiter`]) are separate sample
//! types so destination vs sender hashing cannot be confused. Layout matches
//! the SHM sample (`repr(C)` + [`ZeroCopySend`]).

use std::fmt;

use iceoryx2::prelude::ZeroCopySend;

/// Host-initiated stop reasons on the control plane (no free-form strings).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum HostStopReason {
    Breakpoint = 1,
    ExternalStop = 2,
    Step = 3,
    Unmapped = 4,
}

impl HostStopReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Breakpoint => "breakpoint",
            Self::ExternalStop => "external_stop",
            Self::Step => "step",
            Self::Unmapped => "unmapped",
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
            _ => None,
        }
    }
}

impl fmt::Display for HostStopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Logical destination for arbiter → node (a2n) messages.
///
/// Physical delivery may still be pub/sub broadcast; receivers use
/// [`BoardTarget::includes`] (or [`ControlToNode::is_for`]) to accept or
/// silently drop.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum BoardTarget {
    /// Every subscriber should process the message.
    Broadcast,
    /// Only the board with this hash should process the message.
    Unicast { board_id_hash: u64 },
}

impl BoardTarget {
    /// Whether `board_id_hash` is in the logical destination set.
    #[must_use]
    pub fn includes(self, board_id_hash: u64) -> bool {
        match self {
            Self::Broadcast => true,
            Self::Unicast {
                board_id_hash: target,
            } => target == board_id_hash,
        }
    }
}

/// Arbiter → node control messages (a2n iceoryx2 sample type).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum ControlToNode {
    /// Shared startup parameters (always broadcast; see issue #43).
    StartupRecord {
        /// Virtual-time skew margin (ns); copied from topology for the node.
        margin_ns: u64,
        /// Report when `allowed - now` falls below this (ns).
        headroom_threshold_ns: u64,
    },
    /// Simulation may enter Running (always broadcast).
    Start,
    /// New virtual-time ceiling (always broadcast).
    Allowed { allowed_ns: u64 },
    /// Host-initiated cluster stop (always broadcast; no ack on same host).
    ClusterStop { reason: HostStopReason },
}

impl ControlToNode {
    /// Logical destination of this a2n message.
    #[must_use]
    pub fn destination(self) -> BoardTarget {
        match self {
            Self::StartupRecord { .. }
            | Self::Start
            | Self::Allowed { .. }
            | Self::ClusterStop { .. } => BoardTarget::Broadcast,
        }
    }

    /// Whether this node should process the message.
    #[must_use]
    pub fn is_for(self, board_id_hash: u64) -> bool {
        self.destination().includes(board_id_hash)
    }
}

/// Node → arbiter control messages (n2a iceoryx2 sample type).
///
/// `from` is always the sender board hash (not a destination).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum ControlToArbiter {
    /// Control attach complete.
    Ready { from: u64 },
    /// Virtual time report.
    TimeReport { from: u64, virtual_time_ns: u64 },
    /// Host-initiated stop (BP / external / step / unmapped).
    /// Guest-local Halt must not be sent.
    HostStop { from: u64, reason: HostStopReason },
}

impl ControlToArbiter {
    /// Sender board hash.
    #[must_use]
    pub fn from_board(self) -> u64 {
        match self {
            Self::Ready { from } | Self::TimeReport { from, .. } | Self::HostStop { from, .. } => {
                from
            }
        }
    }
}
