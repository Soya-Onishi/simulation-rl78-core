//! Event-driven control-plane state machines (arbiter peers and nodes).
//!
//! Unexpected messages do not fail the cluster: they log a warning and leave
//! the state unchanged. Ready-barrier timeout remains a hard failure at the
//! arbiter orchestration layer (simulation must not start).

use crate::control::{ControlToArbiter, ControlToNode, HostStopReason};

/// Per-node control-plane state.
///
/// Starts in [`Stopped`](Self::Stopped) — the same state as after a breakpoint
/// / [`ControlToNode::ClusterStop`]. Simulation work runs only in [`Running`](Self::Running).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// Idle: waiting for Start, or halted after ClusterStop / host stop.
    Stopped,
    /// Simulation may run.
    Running,
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
    /// Returns the next state and an optional effect. Unexpected messages yield
    /// [`NodeEffect::Warn`] and keep the current state.
    #[must_use]
    pub fn on_message(self, msg: ControlToNode) -> (Self, Option<NodeEffect>) {
        match (self, msg) {
            (NodeState::Stopped, ControlToNode::StartupRecord { .. }) => {
                (NodeState::Stopped, Some(NodeEffect::SendReady))
            }
            (NodeState::Stopped, ControlToNode::Start) => (NodeState::Running, None),
            (NodeState::Running, ControlToNode::Allowed { allowed_ns }) => (
                NodeState::Running,
                Some(NodeEffect::SetAllowed { allowed_ns }),
            ),
            (NodeState::Running, ControlToNode::ClusterStop { reason }) => (
                NodeState::Stopped,
                Some(NodeEffect::Warn(format!("ClusterStop ({reason})"))),
            ),
            // Already idle (e.g. duplicate ClusterStop after breakpoint).
            (NodeState::Stopped, ControlToNode::ClusterStop { .. }) => (NodeState::Stopped, None),
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
    /// Apply one inbound n2a message already routed to this peer.
    ///
    /// Caller must select the peer via [`ControlToArbiter::from_board`]; this
    /// method does not re-check sender identity.
    #[must_use]
    pub fn on_message(self, msg: ControlToArbiter) -> (Self, Option<PeerEffect>) {
        match (self, msg) {
            (PeerState::AwaitingReady, ControlToArbiter::Ready { .. }) => (PeerState::Ready, None),
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
            ) => (
                PeerState::Running,
                Some(PeerEffect::TimeReport {
                    from,
                    virtual_time_ns,
                }),
            ),
            (PeerState::Running, ControlToArbiter::HostStop { from, reason }) => (
                PeerState::Stopped,
                Some(PeerEffect::HostStop { from, reason }),
            ),
            (state, msg) => (
                state,
                Some(PeerEffect::Warn(format!(
                    "ignoring {msg:?} in state {state:?}"
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
        let mut s = NodeState::Stopped;
        let (n, eff) = s.on_message(ControlToNode::StartupRecord {
            margin_ns: 1000,
            headroom_threshold_ns: 100,
        });
        assert_eq!(n, NodeState::Stopped);
        assert_eq!(eff, Some(NodeEffect::SendReady));
        s = n;
        let (n, eff) = s.on_message(ControlToNode::Start);
        assert_eq!(n, NodeState::Running);
        assert!(eff.is_none());
    }

    #[test]
    fn node_start_from_stopped_after_cluster_stop() {
        let mut s = NodeState::Running;
        let (n, _) = s.on_message(ControlToNode::ClusterStop {
            reason: HostStopReason::Breakpoint,
        });
        assert_eq!(n, NodeState::Stopped);
        s = n;
        let (n, eff) = s.on_message(ControlToNode::Start);
        assert_eq!(n, NodeState::Running);
        assert!(eff.is_none());
    }

    #[test]
    fn node_ignores_startup_after_running() {
        let s = NodeState::Running;
        let (n, eff) = s.on_message(ControlToNode::StartupRecord {
            margin_ns: 1000,
            headroom_threshold_ns: 100,
        });
        assert_eq!(n, NodeState::Running);
        assert!(matches!(eff, Some(NodeEffect::Warn(_))));
    }

    #[test]
    fn peer_ready_and_duplicate() {
        let hash_a = board_id_hash("a");
        let mut s = PeerState::AwaitingReady;
        let (n, eff) = s.on_message(ControlToArbiter::Ready { from: hash_a });
        assert_eq!(n, PeerState::Ready);
        assert!(eff.is_none());
        s = n;
        let (n, eff) = s.on_message(ControlToArbiter::Ready { from: hash_a });
        assert_eq!(n, PeerState::Ready);
        assert!(matches!(eff, Some(PeerEffect::Warn(_))));
    }

    #[test]
    fn peer_host_stop_while_running() {
        let hash_a = board_id_hash("a");
        let s = PeerState::Running;
        let (n, eff) = s.on_message(ControlToArbiter::HostStop {
            from: hash_a,
            reason: HostStopReason::Breakpoint,
        });
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
        let (n, eff) = s.on_message(ControlToNode::ClusterStop {
            reason: HostStopReason::Breakpoint,
        });
        assert_eq!(n, NodeState::Stopped);
        assert!(matches!(eff, Some(NodeEffect::Warn(_))));
    }
}
