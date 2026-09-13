//! Control-plane messages shared by arbiter, nodes, and iceoryx2 pub/sub.
//!
//! Layout matches the SHM sample (`repr(C)` + [`ZeroCopySend`]): board ids are
//! hashes and stop reasons are a fixed enum — no separate wire type.

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

/// Control messages between arbiter and nodes (also the iceoryx2 sample type).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum ControlMessage {
    /// Arbiter → node: bind resources / time parameters for this board.
    StartupRecord {
        board_id_hash: u64,
        /// Virtual-time skew margin (ns); copied from topology for the node.
        margin_ns: u64,
        /// Report when `allowed - now` falls below this (ns).
        headroom_threshold_ns: u64,
    },
    /// Node → arbiter: control attach complete.
    Ready { board_id_hash: u64 },
    /// Arbiter → node: simulation may enter Running.
    Start,
    /// Node → arbiter: virtual time report.
    TimeReport {
        board_id_hash: u64,
        virtual_time_ns: u64,
    },
    /// Arbiter → node: new virtual-time ceiling.
    Allowed { allowed_ns: u64 },
    /// Node → arbiter: host-initiated stop (BP / external / step / unmapped).
    /// Guest-local Halt must not be sent.
    HostStop {
        board_id_hash: u64,
        reason: HostStopReason,
    },
    /// Arbiter → node: host-initiated cluster stop (no ack on same host).
    ClusterStop { reason: HostStopReason },
}
