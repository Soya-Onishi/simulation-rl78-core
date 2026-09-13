//! Event-driven control-plane state machines (arbiter peers and nodes).
//!
//! Unexpected messages do not fail the cluster: they log a warning and leave
//! the state unchanged. Ready-barrier timeout remains a hard failure at the
//! arbiter orchestration layer (simulation must not start).

use crate::control::{ControlMessage, HostStopReason};

/// Per-node control-plane state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// Waiting for the first usable [`ControlMessage::StartupRecord`].
    AwaitingStartup,
    /// Ready sent; waiting for [`ControlMessage::Start`].
    AwaitingStart,
    /// Simulation may run.
    Running,
    /// Host-initiated or local stop.
    Stopped,
}

/// Side effects requested by [`NodeState::on_message`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeEffect {
    /// Send [`ControlMessage::Ready`] to the arbiter.
    SendReady,
    /// Apply a new virtual-time ceiling locally.
    SetAllowed { allowed_ns: u64 },
    /// Log-only; state is unchanged aside from the warning.
    Warn(String),
}

impl NodeState {
    /// Apply one inbound control message.
    ///
    /// Returns the next state and an optional effect. Unexpected messages yield
    /// [`NodeEffect::Warn`] and keep the current state.
    #[must_use]
    pub fn on_message(self, board_id_hash: u64, msg: ControlMessage) -> (Self, Option<NodeEffect>) {
        match (self, msg) {
            (
                NodeState::AwaitingStartup,
                ControlMessage::StartupRecord {
                    board_id_hash: id_hash,
                    ..
                },
            ) => {
                if id_hash != board_id_hash {
                    return (
                        self,
                        Some(NodeEffect::Warn(format!(
                            "StartupRecord board_id_hash {id_hash:#x} != local {board_id_hash:#x}; ignoring"
                        ))),
                    );
                }
                (NodeState::AwaitingStart, Some(NodeEffect::SendReady))
            }
            (NodeState::AwaitingStart, ControlMessage::Start) => (NodeState::Running, None),
            (NodeState::Running, ControlMessage::Allowed { allowed_ns }) => (
                NodeState::Running,
                Some(NodeEffect::SetAllowed { allowed_ns }),
            ),
            (
                NodeState::Running | NodeState::AwaitingStart | NodeState::AwaitingStartup,
                ControlMessage::ClusterStop { reason },
            ) => (
                NodeState::Stopped,
                Some(NodeEffect::Warn(format!("ClusterStop ({reason})"))),
            ),
            // Duplicate / late / early messages: warn and stay.
            (state, msg) => (
                state,
                Some(NodeEffect::Warn(format!(
                    "ignoring {msg:?} in state {state:?}"
                ))),
            ),
        }
    }
}

/// Per-board peer state on the arbiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerState {
    /// StartupRecord sent (or about to be); waiting for Ready.
    AwaitingReady,
    /// Ready received.
    Ready,
    /// Start broadcast done for this peer.
    Running,
    /// Stopped (ClusterStop sent or connection lost).
    Stopped,
}

/// Side effects for arbiter peer transitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerEffect {
    Warn(String),
    TimeReport {
        board_id_hash: u64,
        virtual_time_ns: u64,
    },
    HostStop {
        board_id_hash: u64,
        reason: HostStopReason,
    },
}

impl PeerState {
    /// Apply one inbound message from this peer.
    #[must_use]
    pub fn on_message(
        self,
        expected_board_hash: u64,
        msg: ControlMessage,
    ) -> (Self, Option<PeerEffect>) {
        match (self, msg) {
            (PeerState::AwaitingReady, ControlMessage::Ready { board_id_hash }) => {
                if board_id_hash != expected_board_hash {
                    return (
                        self,
                        Some(PeerEffect::Warn(format!(
                            "Ready board_id_hash {board_id_hash:#x} != expected {expected_board_hash:#x}; ignoring"
                        ))),
                    );
                }
                (PeerState::Ready, None)
            }
            (PeerState::Ready, ControlMessage::Ready { board_id_hash }) => (
                PeerState::Ready,
                Some(PeerEffect::Warn(format!(
                    "duplicate Ready from {board_id_hash:#x}; ignoring"
                ))),
            ),
            (
                PeerState::Running,
                ControlMessage::TimeReport {
                    board_id_hash,
                    virtual_time_ns,
                },
            ) => {
                if board_id_hash != expected_board_hash {
                    return (
                        self,
                        Some(PeerEffect::Warn(format!(
                            "TimeReport board_id_hash {board_id_hash:#x} != {expected_board_hash:#x}; ignoring"
                        ))),
                    );
                }
                (
                    PeerState::Running,
                    Some(PeerEffect::TimeReport {
                        board_id_hash,
                        virtual_time_ns,
                    }),
                )
            }
            (
                PeerState::Running,
                ControlMessage::HostStop {
                    board_id_hash,
                    reason,
                },
            ) => {
                if board_id_hash != expected_board_hash {
                    return (
                        self,
                        Some(PeerEffect::Warn(format!(
                            "HostStop board_id_hash {board_id_hash:#x} != {expected_board_hash:#x}; ignoring"
                        ))),
                    );
                }
                (
                    PeerState::Stopped,
                    Some(PeerEffect::HostStop {
                        board_id_hash,
                        reason,
                    }),
                )
            }
            (state, msg) => (
                state,
                Some(PeerEffect::Warn(format!(
                    "ignoring {msg:?} from {expected_board_hash:#x} in state {state:?}"
                ))),
            ),
        }
    }

    /// Mark this peer as started after Start broadcast.
    #[must_use]
    pub fn on_start_broadcast(self) -> Self {
        match self {
            PeerState::Ready | PeerState::Running => PeerState::Running,
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::board_id_hash;

    #[test]
    fn node_happy_path() {
        let hash_a = board_id_hash("a");
        let mut s = NodeState::AwaitingStartup;
        let (n, eff) = s.on_message(
            hash_a,
            ControlMessage::StartupRecord {
                board_id_hash: hash_a,
                margin_ns: 1000,
                headroom_threshold_ns: 100,
            },
        );
        assert_eq!(n, NodeState::AwaitingStart);
        assert_eq!(eff, Some(NodeEffect::SendReady));
        s = n;
        let (n, eff) = s.on_message(hash_a, ControlMessage::Start);
        assert_eq!(n, NodeState::Running);
        assert!(eff.is_none());
    }

    #[test]
    fn node_ignores_startup_after_running() {
        let hash_a = board_id_hash("a");
        let s = NodeState::Running;
        let (n, eff) = s.on_message(
            hash_a,
            ControlMessage::StartupRecord {
                board_id_hash: hash_a,
                margin_ns: 1000,
                headroom_threshold_ns: 100,
            },
        );
        assert_eq!(n, NodeState::Running);
        assert!(matches!(eff, Some(NodeEffect::Warn(_))));
    }

    #[test]
    fn peer_ready_and_duplicate() {
        let hash_a = board_id_hash("a");
        let mut s = PeerState::AwaitingReady;
        let (n, eff) = s.on_message(
            hash_a,
            ControlMessage::Ready {
                board_id_hash: hash_a,
            },
        );
        assert_eq!(n, PeerState::Ready);
        assert!(eff.is_none());
        s = n;
        let (n, eff) = s.on_message(
            hash_a,
            ControlMessage::Ready {
                board_id_hash: hash_a,
            },
        );
        assert_eq!(n, PeerState::Ready);
        assert!(matches!(eff, Some(PeerEffect::Warn(_))));
    }

    #[test]
    fn peer_host_stop_while_running() {
        let hash_a = board_id_hash("a");
        let s = PeerState::Running;
        let (n, eff) = s.on_message(
            hash_a,
            ControlMessage::HostStop {
                board_id_hash: hash_a,
                reason: HostStopReason::Breakpoint,
            },
        );
        assert_eq!(n, PeerState::Stopped);
        assert_eq!(
            eff,
            Some(PeerEffect::HostStop {
                board_id_hash: hash_a,
                reason: HostStopReason::Breakpoint,
            })
        );
    }

    #[test]
    fn node_cluster_stop_while_running() {
        let s = NodeState::Running;
        let (n, eff) = s.on_message(
            board_id_hash("b"),
            ControlMessage::ClusterStop {
                reason: HostStopReason::Breakpoint,
            },
        );
        assert_eq!(n, NodeState::Stopped);
        assert!(matches!(eff, Some(NodeEffect::Warn(_))));
    }
}
