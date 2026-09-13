//! Event-driven control-plane state machines (arbiter peers and nodes).
//!
//! Unexpected messages do not fail the cluster: they log a warning and leave
//! the state unchanged. Ready-barrier timeout remains a hard failure at the
//! arbiter orchestration layer (simulation must not start). Destination
//! mismatch on a2n Unicast is dropped silently (no warn).

use crate::control::{ControlToArbiter, ControlToNode, HostStopReason};

/// Per-node control-plane state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// Waiting for the first usable [`ControlToNode::StartupRecord`].
    AwaitingStartup,
    /// Ready sent; waiting for [`ControlToNode::Start`].
    AwaitingStart,
    /// Simulation may run.
    Running,
    /// Host-initiated or local stop.
    Stopped,
}

/// Side effects requested by [`NodeState::on_message`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeEffect {
    /// Send [`ControlToArbiter::Ready`] to the arbiter.
    SendReady,
    /// Apply a new virtual-time ceiling locally.
    SetAllowed { allowed_ns: u64 },
    /// Log-only; state is unchanged aside from the warning.
    Warn(String),
}

impl NodeState {
    /// Apply one inbound a2n control message.
    ///
    /// Returns the next state and an optional effect. Messages not addressed to
    /// this board are ignored silently. Other unexpected messages yield
    /// [`NodeEffect::Warn`] and keep the current state.
    #[must_use]
    pub fn on_message(self, board_id_hash: u64, msg: ControlToNode) -> (Self, Option<NodeEffect>) {
        if !msg.is_for(board_id_hash) {
            return (self, None);
        }
        match (self, msg) {
            (NodeState::AwaitingStartup, ControlToNode::StartupRecord { .. }) => {
                (NodeState::AwaitingStart, Some(NodeEffect::SendReady))
            }
            (NodeState::AwaitingStart, ControlToNode::Start) => (NodeState::Running, None),
            (NodeState::Running, ControlToNode::Allowed { allowed_ns }) => (
                NodeState::Running,
                Some(NodeEffect::SetAllowed { allowed_ns }),
            ),
            (
                NodeState::Running | NodeState::AwaitingStart | NodeState::AwaitingStartup,
                ControlToNode::ClusterStop { reason },
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
    TimeReport { from: u64, virtual_time_ns: u64 },
    HostStop { from: u64, reason: HostStopReason },
}

impl PeerState {
    /// Apply one inbound n2a message from this peer.
    #[must_use]
    pub fn on_message(
        self,
        expected_board_hash: u64,
        msg: ControlToArbiter,
    ) -> (Self, Option<PeerEffect>) {
        match (self, msg) {
            (PeerState::AwaitingReady, ControlToArbiter::Ready { from }) => {
                if from != expected_board_hash {
                    return (
                        self,
                        Some(PeerEffect::Warn(format!(
                            "Ready from {from:#x} != expected {expected_board_hash:#x}; ignoring"
                        ))),
                    );
                }
                (PeerState::Ready, None)
            }
            (PeerState::Ready, ControlToArbiter::Ready { from }) => (
                PeerState::Ready,
                Some(PeerEffect::Warn(format!(
                    "duplicate Ready from {from:#x}; ignoring"
                ))),
            ),
            (
                PeerState::Running,
                ControlToArbiter::TimeReport {
                    from,
                    virtual_time_ns,
                },
            ) => {
                if from != expected_board_hash {
                    return (
                        self,
                        Some(PeerEffect::Warn(format!(
                            "TimeReport from {from:#x} != {expected_board_hash:#x}; ignoring"
                        ))),
                    );
                }
                (
                    PeerState::Running,
                    Some(PeerEffect::TimeReport {
                        from,
                        virtual_time_ns,
                    }),
                )
            }
            (PeerState::Running, ControlToArbiter::HostStop { from, reason }) => {
                if from != expected_board_hash {
                    return (
                        self,
                        Some(PeerEffect::Warn(format!(
                            "HostStop from {from:#x} != {expected_board_hash:#x}; ignoring"
                        ))),
                    );
                }
                (
                    PeerState::Stopped,
                    Some(PeerEffect::HostStop { from, reason }),
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
    use crate::control::BoardTarget;
    use crate::ipc::board_id_hash;

    #[test]
    fn node_happy_path() {
        let hash_a = board_id_hash("a");
        let mut s = NodeState::AwaitingStartup;
        let (n, eff) = s.on_message(
            hash_a,
            ControlToNode::StartupRecord {
                target: BoardTarget::Broadcast,
                margin_ns: 1000,
                headroom_threshold_ns: 100,
            },
        );
        assert_eq!(n, NodeState::AwaitingStart);
        assert_eq!(eff, Some(NodeEffect::SendReady));
        s = n;
        let (n, eff) = s.on_message(hash_a, ControlToNode::Start);
        assert_eq!(n, NodeState::Running);
        assert!(eff.is_none());
    }

    #[test]
    fn node_silently_ignores_other_board_unicast() {
        let hash_a = board_id_hash("a");
        let hash_b = board_id_hash("b");
        let s = NodeState::AwaitingStartup;
        let (n, eff) = s.on_message(
            hash_a,
            ControlToNode::StartupRecord {
                target: BoardTarget::Unicast {
                    board_id_hash: hash_b,
                },
                margin_ns: 1000,
                headroom_threshold_ns: 100,
            },
        );
        assert_eq!(n, NodeState::AwaitingStartup);
        assert!(eff.is_none());
    }

    #[test]
    fn node_ignores_startup_after_running() {
        let hash_a = board_id_hash("a");
        let s = NodeState::Running;
        let (n, eff) = s.on_message(
            hash_a,
            ControlToNode::StartupRecord {
                target: BoardTarget::Broadcast,
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
        let (n, eff) = s.on_message(hash_a, ControlToArbiter::Ready { from: hash_a });
        assert_eq!(n, PeerState::Ready);
        assert!(eff.is_none());
        s = n;
        let (n, eff) = s.on_message(hash_a, ControlToArbiter::Ready { from: hash_a });
        assert_eq!(n, PeerState::Ready);
        assert!(matches!(eff, Some(PeerEffect::Warn(_))));
    }

    #[test]
    fn peer_host_stop_while_running() {
        let hash_a = board_id_hash("a");
        let s = PeerState::Running;
        let (n, eff) = s.on_message(
            hash_a,
            ControlToArbiter::HostStop {
                from: hash_a,
                reason: HostStopReason::Breakpoint,
            },
        );
        assert_eq!(n, PeerState::Stopped);
        assert_eq!(
            eff,
            Some(PeerEffect::HostStop {
                from: hash_a,
                reason: HostStopReason::Breakpoint,
            })
        );
    }

    #[test]
    fn node_cluster_stop_while_running() {
        let s = NodeState::Running;
        let (n, eff) = s.on_message(
            board_id_hash("b"),
            ControlToNode::ClusterStop {
                reason: HostStopReason::Breakpoint,
            },
        );
        assert_eq!(n, NodeState::Stopped);
        assert!(matches!(eff, Some(NodeEffect::Warn(_))));
    }
}
