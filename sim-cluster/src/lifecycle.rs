//! Event-driven control-plane state machines (arbiter peers and nodes).
//!
//! Unexpected messages do not fail the cluster: they log a warning and leave
//! the state unchanged. Ready-barrier timeout remains a hard failure at the
//! arbiter orchestration layer (simulation must not start).

use crate::control::ControlMessage;

/// Per-node control-plane state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// Connected; waiting for the first usable [`ControlMessage::StartupRecord`].
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
    /// Log-only; state is unchanged aside from the warning.
    Warn(String),
}

impl NodeState {
    /// Apply one inbound control message.
    ///
    /// Returns the next state and an optional effect. Unexpected messages yield
    /// [`NodeEffect::Warn`] and keep the current state.
    #[must_use]
    pub fn on_message(self, board_id: &str, msg: ControlMessage) -> (Self, Option<NodeEffect>) {
        match (self, msg) {
            (
                NodeState::AwaitingStartup,
                ControlMessage::StartupRecord {
                    board_id: id,
                    shm_segments,
                },
            ) => {
                if id != board_id {
                    return (
                        self,
                        Some(NodeEffect::Warn(format!(
                            "StartupRecord board_id '{id}' != local '{board_id}'; ignoring"
                        ))),
                    );
                }
                let _ = shm_segments; // Phase 3: attach SHM before Ready
                (NodeState::AwaitingStart, Some(NodeEffect::SendReady))
            }
            (NodeState::AwaitingStart, ControlMessage::Start) => (NodeState::Running, None),
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
    /// Listener bound; waiting for the node to connect.
    Accepting,
    /// Connected and StartupRecord sent; waiting for Ready.
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
}

impl PeerState {
    /// Node connected on this board's control socket.
    #[must_use]
    pub fn on_connected(self) -> (Self, Option<PeerEffect>) {
        match self {
            PeerState::Accepting => (PeerState::AwaitingReady, None),
            other => (
                other,
                Some(PeerEffect::Warn(format!(
                    "unexpected connect while {other:?}"
                ))),
            ),
        }
    }

    /// Apply one inbound message from this peer.
    #[must_use]
    pub fn on_message(
        self,
        expected_board: &str,
        msg: ControlMessage,
    ) -> (Self, Option<PeerEffect>) {
        match (self, msg) {
            (PeerState::AwaitingReady, ControlMessage::Ready { board_id }) => {
                if board_id != expected_board {
                    return (
                        self,
                        Some(PeerEffect::Warn(format!(
                            "Ready board_id '{board_id}' != socket '{expected_board}'; ignoring"
                        ))),
                    );
                }
                (PeerState::Ready, None)
            }
            (PeerState::Ready, ControlMessage::Ready { board_id }) => (
                PeerState::Ready,
                Some(PeerEffect::Warn(format!(
                    "duplicate Ready from '{board_id}'; ignoring"
                ))),
            ),
            (PeerState::Running, ControlMessage::TimeReport { .. }) => {
                // Phase 4 will act on this; for now accept silently.
                (PeerState::Running, None)
            }
            (state, msg) => (
                state,
                Some(PeerEffect::Warn(format!(
                    "ignoring {msg:?} from '{expected_board}' in state {state:?}"
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

    #[test]
    fn node_happy_path() {
        let mut s = NodeState::AwaitingStartup;
        let (n, eff) = s.on_message(
            "a",
            ControlMessage::StartupRecord {
                board_id: "a".into(),
                shm_segments: vec![],
            },
        );
        assert_eq!(n, NodeState::AwaitingStart);
        assert_eq!(eff, Some(NodeEffect::SendReady));
        s = n;
        let (n, eff) = s.on_message("a", ControlMessage::Start);
        assert_eq!(n, NodeState::Running);
        assert!(eff.is_none());
    }

    #[test]
    fn node_ignores_startup_after_running() {
        let s = NodeState::Running;
        let (n, eff) = s.on_message(
            "a",
            ControlMessage::StartupRecord {
                board_id: "a".into(),
                shm_segments: vec![],
            },
        );
        assert_eq!(n, NodeState::Running);
        assert!(matches!(eff, Some(NodeEffect::Warn(_))));
    }

    #[test]
    fn peer_ready_and_duplicate() {
        let mut s = PeerState::Accepting;
        let (n, _) = s.on_connected();
        assert_eq!(n, PeerState::AwaitingReady);
        s = n;
        let (n, eff) = s.on_message(
            "a",
            ControlMessage::Ready {
                board_id: "a".into(),
            },
        );
        assert_eq!(n, PeerState::Ready);
        assert!(eff.is_none());
        s = n;
        let (n, eff) = s.on_message(
            "a",
            ControlMessage::Ready {
                board_id: "a".into(),
            },
        );
        assert_eq!(n, PeerState::Ready);
        assert!(matches!(eff, Some(PeerEffect::Warn(_))));
    }
}
