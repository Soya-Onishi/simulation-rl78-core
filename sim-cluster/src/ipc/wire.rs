//! Zero-copy wire payloads for cluster control and UART.

use iceoryx2::prelude::ZeroCopySend;

use crate::cluster_stop::is_cluster_relevant_reason;
use crate::control::ControlMessage;

use super::hash::board_id_hash;

/// Host-initiated stop reasons carried on the wire (no free-form strings).
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

/// Packed UART frame (host endian).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ZeroCopySend)]
pub struct UartFrame {
    pub data: u16,
    pub data_bits: u8,
    pub stop_bits: u8,
    pub parity: u8,
    pub inverted: u8,
    pub _pad: u16,
    pub bit_time_ns: u64,
}

/// Control-plane sample on a2n / n2a pubsub.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum ControlWire {
    StartupRecord {
        board_id_hash: u64,
        margin_ns: u64,
        headroom_threshold_ns: u64,
    },
    Ready {
        board_id_hash: u64,
    },
    Start,
    TimeReport {
        board_id_hash: u64,
        virtual_time_ns: u64,
    },
    Allowed {
        allowed_ns: u64,
    },
    HostStop {
        board_id_hash: u64,
        reason: HostStopReason,
    },
    ClusterStop {
        reason: HostStopReason,
    },
}

impl ControlWire {
    /// Encode a logical control message. Returns `None` if reason is not cluster-wireable.
    #[must_use]
    pub fn from_logical(msg: &ControlMessage) -> Option<Self> {
        Some(match msg {
            ControlMessage::StartupRecord {
                board_id,
                margin_ns,
                headroom_threshold_ns,
            } => Self::StartupRecord {
                board_id_hash: board_id_hash(board_id),
                margin_ns: *margin_ns,
                headroom_threshold_ns: *headroom_threshold_ns,
            },
            ControlMessage::Ready { board_id } => Self::Ready {
                board_id_hash: board_id_hash(board_id),
            },
            ControlMessage::Start => Self::Start,
            ControlMessage::TimeReport {
                board_id,
                virtual_time_ns,
            } => Self::TimeReport {
                board_id_hash: board_id_hash(board_id),
                virtual_time_ns: *virtual_time_ns,
            },
            ControlMessage::Allowed { allowed_ns } => Self::Allowed {
                allowed_ns: *allowed_ns,
            },
            ControlMessage::HostStop { board_id, reason } => {
                let reason = HostStopReason::from_label(reason)?;
                if !is_cluster_relevant_reason(reason.as_str()) {
                    return None;
                }
                Self::HostStop {
                    board_id_hash: board_id_hash(board_id),
                    reason,
                }
            }
            ControlMessage::ClusterStop { reason } => {
                let reason = HostStopReason::from_label(reason)?;
                Self::ClusterStop { reason }
            }
        })
    }

    /// Decode using a board hash → id table (arbiter). Nodes may pass a single-entry map.
    #[must_use]
    pub fn to_logical(&self, boards: &std::collections::HashMap<u64, String>) -> Option<ControlMessage> {
        match *self {
            Self::StartupRecord {
                board_id_hash,
                margin_ns,
                headroom_threshold_ns,
            } => {
                let board_id = boards.get(&board_id_hash)?.clone();
                Some(ControlMessage::StartupRecord {
                    board_id,
                    margin_ns,
                    headroom_threshold_ns,
                })
            }
            Self::Ready { board_id_hash } => {
                let board_id = boards.get(&board_id_hash)?.clone();
                Some(ControlMessage::Ready { board_id })
            }
            Self::Start => Some(ControlMessage::Start),
            Self::TimeReport {
                board_id_hash,
                virtual_time_ns,
            } => {
                let board_id = boards.get(&board_id_hash)?.clone();
                Some(ControlMessage::TimeReport {
                    board_id,
                    virtual_time_ns,
                })
            }
            Self::Allowed { allowed_ns } => Some(ControlMessage::Allowed { allowed_ns }),
            Self::HostStop {
                board_id_hash,
                reason,
            } => {
                let board_id = boards.get(&board_id_hash)?.clone();
                Some(ControlMessage::HostStop {
                    board_id,
                    reason: reason.as_str().to_string(),
                })
            }
            Self::ClusterStop { reason } => Some(ControlMessage::ClusterStop {
                reason: reason.as_str().to_string(),
            }),
        }
    }
}
