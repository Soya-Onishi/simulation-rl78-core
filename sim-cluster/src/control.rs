//! Logical control-plane messages (in-process). Wire encoding lives in [`crate::ipc::wire`].

/// Control messages between arbiter and nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlMessage {
    /// Arbiter → node: bind resources / time parameters for this board.
    StartupRecord {
        board_id: String,
        /// Virtual-time skew margin (ns); copied from topology for the node.
        margin_ns: u64,
        /// Report when `allowed - now` falls below this (ns).
        headroom_threshold_ns: u64,
    },
    /// Node → arbiter: control attach complete.
    Ready { board_id: String },
    /// Arbiter → node: simulation may enter Running.
    Start,
    /// Node → arbiter: virtual time report.
    TimeReport {
        board_id: String,
        virtual_time_ns: u64,
    },
    /// Arbiter → node: new virtual-time ceiling.
    Allowed { allowed_ns: u64 },
    /// Node → arbiter: host-initiated stop (BP / external / step / unmapped).
    /// Guest-local Halt must not be sent.
    HostStop { board_id: String, reason: String },
    /// Arbiter → node: host-initiated cluster stop (no ack on same host).
    ClusterStop { reason: String },
}
