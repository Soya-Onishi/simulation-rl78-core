//! Event-driven control-plane state machines (arbiter peers and nodes).
//!
//! Unexpected messages do not fail the cluster: they log a warning and leave
//! the state unchanged. Booting-barrier timeout remains a hard failure at the
//! arbiter orchestration layer (simulation must not start).

use crate::control::{ControlToArbiter, ControlToNode, HostStopReason, NodeStateWire};

/// Per-node control-plane state.
///
/// Process init is conceptually [`Booting`](Self::Booting). Once the control
/// loop starts, the node is [`Stopped`](Self::Stopped) — the same idle state as
/// after a breakpoint / [`ControlToNode::ClusterStop`]. Simulation work runs
/// only in [`Running`](Self::Running).
///
/// Arbiter peers use [`PeerState::Booting`] until the first
/// [`ControlToArbiter::State`] arrives; nodes normally report [`Stopped`] after
/// init, so [`Booting`](Self::Booting) is mainly a wire / peer label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// Process init before the control loop (rarely reported on the wire).
    Booting,
    /// Idle: waiting for Start, or halted after ClusterStop / host stop.
    Stopped,
    /// Simulation may run.
    Running,
}

impl NodeState {
    /// Encode for [`ControlToArbiter::State`].
    #[must_use]
    pub fn to_wire(self) -> NodeStateWire {
        match self {
            Self::Booting => NodeStateWire::Booting,
            Self::Stopped => NodeStateWire::Stopped,
            Self::Running => NodeStateWire::Running,
        }
    }

    /// Decode a [`NodeStateWire`] sample.
    #[must_use]
    pub fn from_wire(wire: NodeStateWire) -> Self {
        match wire {
            NodeStateWire::Booting => Self::Booting,
            NodeStateWire::Stopped => Self::Stopped,
            NodeStateWire::Running => Self::Running,
        }
    }
}

/// Side effects requested by [`NodeState::on_message`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeEffect {
    /// Reply to [`ControlToNode::GetState`] with the current [`NodeState`].
    ReportState,
    /// Apply a new virtual-time ceiling locally.
    SetAllowed { allowed_ns: u64 },
    /// Hold-reset devices and the CPU via [`sim_kernel::Machine::reset`].
    Reset,
    /// Replace the probe emit set for `board_hash` (ignore if it is not this node).
    ProbeWatch {
        board_hash: crate::ipc::HashedId<u64>,
        probe_ids: Vec<crate::ipc::HashedId<u64>>,
    },
    /// Execute one kernel [`sim_kernel::Command::Step`] if `board_hash` matches.
    GdbStep {
        board_hash: crate::ipc::HashedId<u64>,
    },
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
            (state, ControlToNode::GetState) => (state, Some(NodeEffect::ReportState)),
            (NodeState::Stopped | NodeState::Running, ControlToNode::Start { allowed_ns }) => (
                NodeState::Running,
                Some(NodeEffect::SetAllowed { allowed_ns }),
            ),
            // Ceiling may be republished while stopped, immediately after Reset
            // and before the following Start.
            (
                state @ (NodeState::Stopped | NodeState::Running),
                ControlToNode::Allowed { allowed_ns },
            ) => (state, Some(NodeEffect::SetAllowed { allowed_ns })),
            (NodeState::Stopped, ControlToNode::Reset) => {
                (NodeState::Stopped, Some(NodeEffect::Reset))
            }
            (NodeState::Running, ControlToNode::ClusterStop { reason }) => (
                NodeState::Stopped,
                Some(NodeEffect::Warn(format!("ClusterStop ({reason})"))),
            ),
            (NodeState::Booting | NodeState::Stopped, ControlToNode::ClusterStop { .. }) => {
                (NodeState::Stopped, None)
            }
            (
                state,
                ControlToNode::ProbeWatch {
                    board_hash,
                    count,
                    probe_ids,
                },
            ) => (
                state,
                Some(NodeEffect::ProbeWatch {
                    board_hash,
                    probe_ids: probe_ids.iter().copied().take(usize::from(count)).collect(),
                }),
            ),
            (NodeState::Stopped | NodeState::Running, ControlToNode::GdbStep { board_hash }) => {
                (NodeState::Stopped, Some(NodeEffect::GdbStep { board_hash }))
            }
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
    /// Waiting for the first [`ControlToArbiter::State`] (or still reported Booting).
    Booting,
    /// Start broadcast done for this peer.
    Running,
    /// Stopped (ClusterStop sent, or reported Stopped after boot).
    Stopped,
}

impl PeerState {
    /// Map a node-reported wire state onto the arbiter peer view.
    #[must_use]
    pub fn from_node_wire(wire: NodeStateWire) -> Self {
        match NodeState::from_wire(wire) {
            NodeState::Booting => Self::Booting,
            NodeState::Stopped => Self::Stopped,
            NodeState::Running => Self::Running,
        }
    }
}

/// Side effects for arbiter peer transitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerEffect {
    Warn(String),
    TimeReport {
        from: crate::ipc::HashedId<u64>,
        virtual_time_ns: u64,
    },
    HostStop {
        from: crate::ipc::HashedId<u64>,
        reason: HostStopReason,
    },
}

impl PeerState {
    /// Apply one inbound n2a message already routed to this peer.
    ///
    /// Caller must select the peer via [`ControlToArbiter::from_board`]; this
    /// method does not re-check sender identity.
    ///
    /// [`ControlToArbiter::State`] is applied during the Booting barrier via
    /// [`Self::from_node_wire`]. Late samples after the barrier are ignored
    /// here without a warning (queued replies to earlier GetState).
    #[must_use]
    pub fn on_message(self, msg: ControlToArbiter) -> (Self, Option<PeerEffect>) {
        match (self, msg) {
            (state, ControlToArbiter::State { .. }) => (state, None),
            (
                state @ (PeerState::Booting | PeerState::Running | PeerState::Stopped),
                ControlToArbiter::TimeReport {
                    from,
                    virtual_time_ns,
                },
            ) => (
                state,
                Some(PeerEffect::TimeReport {
                    from,
                    virtual_time_ns,
                }),
            ),
            (
                PeerState::Booting | PeerState::Running | PeerState::Stopped,
                ControlToArbiter::HostStop { from, reason },
            ) => (
                PeerState::Stopped,
                Some(PeerEffect::HostStop { from, reason }),
            ),
        }
    }

    /// Mark this peer as started after Start broadcast.
    #[must_use]
    pub fn on_start_broadcast(self) -> Self {
        match self {
            // Stopped is included so a later host Start resumes the same peers.
            PeerState::Running | PeerState::Stopped => PeerState::Running,
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
        let (n, eff) = s.on_message(ControlToNode::GetState);
        assert_eq!(n, NodeState::Stopped);
        assert_eq!(eff, Some(NodeEffect::ReportState));
        s = n;
        let (n, eff) = s.on_message(ControlToNode::Start { allowed_ns: 0 });
        assert_eq!(n, NodeState::Running);
        assert_eq!(eff, Some(NodeEffect::SetAllowed { allowed_ns: 0 }));
    }

    #[test]
    fn node_get_state_from_running_and_booting() {
        let (n, eff) = NodeState::Running.on_message(ControlToNode::GetState);
        assert_eq!(n, NodeState::Running);
        assert_eq!(eff, Some(NodeEffect::ReportState));
        let (n, eff) = NodeState::Booting.on_message(ControlToNode::GetState);
        assert_eq!(n, NodeState::Booting);
        assert_eq!(eff, Some(NodeEffect::ReportState));
    }

    #[test]
    fn peer_ignores_late_state_without_warn() {
        let (n, eff) = PeerState::Stopped.on_message(ControlToArbiter::State {
            from: board_id_hash("a"),
            state: NodeStateWire::Stopped,
        });
        assert_eq!(n, PeerState::Stopped);
        assert!(eff.is_none());
    }

    #[test]
    fn node_start_while_running_updates_allowed() {
        let (n, eff) = NodeState::Running.on_message(ControlToNode::Start { allowed_ns: 80 });
        assert_eq!(n, NodeState::Running);
        assert_eq!(eff, Some(NodeEffect::SetAllowed { allowed_ns: 80 }));
    }

    #[test]
    fn node_start_from_stopped_after_cluster_stop() {
        let mut s = NodeState::Running;
        let (n, _) = s.on_message(ControlToNode::ClusterStop {
            reason: HostStopReason::Breakpoint,
        });
        assert_eq!(n, NodeState::Stopped);
        s = n;
        let (n, eff) = s.on_message(ControlToNode::Start { allowed_ns: 50 });
        assert_eq!(n, NodeState::Running);
        assert_eq!(eff, Some(NodeEffect::SetAllowed { allowed_ns: 50 }));
    }

    #[test]
    fn node_reset_then_ceiling_then_start() {
        let mut s = NodeState::Stopped;
        let (n, eff) = s.on_message(ControlToNode::Reset);
        assert_eq!(n, NodeState::Stopped);
        assert_eq!(eff, Some(NodeEffect::Reset));
        s = n;
        let (n, eff) = s.on_message(ControlToNode::Allowed { allowed_ns: 1_000 });
        assert_eq!(n, NodeState::Stopped);
        assert_eq!(eff, Some(NodeEffect::SetAllowed { allowed_ns: 1_000 }));
        s = n;
        let (n, eff) = s.on_message(ControlToNode::Start { allowed_ns: 1_000 });
        assert_eq!(n, NodeState::Running);
        assert_eq!(eff, Some(NodeEffect::SetAllowed { allowed_ns: 1_000 }));
    }

    #[test]
    fn peer_start_from_stopped_resumes() {
        assert_eq!(PeerState::Stopped.on_start_broadcast(), PeerState::Running);
        assert_eq!(PeerState::Booting.on_start_broadcast(), PeerState::Booting);
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

    #[test]
    fn node_gdb_step_from_stopped() {
        let (n, eff) = NodeState::Stopped.on_message(ControlToNode::GdbStep {
            board_hash: crate::ipc::HashedId::new(7),
        });
        assert_eq!(n, NodeState::Stopped);
        assert_eq!(
            eff,
            Some(NodeEffect::GdbStep {
                board_hash: crate::ipc::HashedId::new(7)
            })
        );
    }

    #[test]
    fn peer_time_report_while_stopped() {
        let hash_a = board_id_hash("a");
        let (n, eff) = PeerState::Stopped.on_message(ControlToArbiter::TimeReport {
            from: hash_a,
            virtual_time_ns: 50,
        });
        assert_eq!(n, PeerState::Stopped);
        assert_eq!(
            eff,
            Some(PeerEffect::TimeReport {
                from: hash_a,
                virtual_time_ns: 50,
            })
        );
    }

    #[test]
    fn peer_accepts_step_reports_while_stopped() {
        let hash_a = board_id_hash("a");
        let (n, eff) = PeerState::Stopped.on_message(ControlToArbiter::TimeReport {
            from: hash_a,
            virtual_time_ns: 50,
        });
        assert_eq!(n, PeerState::Stopped);
        assert_eq!(
            eff,
            Some(PeerEffect::TimeReport {
                from: hash_a,
                virtual_time_ns: 50,
            })
        );
        let (n, eff) = n.on_message(ControlToArbiter::HostStop {
            from: hash_a,
            reason: HostStopReason::Step,
        });
        assert_eq!(n, PeerState::Stopped);
        assert_eq!(
            eff,
            Some(PeerEffect::HostStop {
                from: hash_a,
                reason: HostStopReason::Step,
            })
        );
    }

    #[test]
    fn node_probe_watch_does_not_change_state() {
        let (n, eff) = NodeState::Running.on_message(ControlToNode::ProbeWatch {
            board_hash: crate::ipc::HashedId::new(1),
            count: 0,
            probe_ids: [crate::ipc::HashedId::new(0); crate::probe::MAX_PROBES_PER_BOARD],
        });
        assert_eq!(n, NodeState::Running);
        assert!(matches!(
            eff,
            Some(NodeEffect::ProbeWatch { probe_ids, .. }) if probe_ids.is_empty()
        ));
    }

    #[test]
    fn peer_from_node_wire() {
        assert_eq!(
            PeerState::from_node_wire(NodeStateWire::Booting),
            PeerState::Booting
        );
        assert_eq!(
            PeerState::from_node_wire(NodeStateWire::Stopped),
            PeerState::Stopped
        );
        assert_eq!(
            PeerState::from_node_wire(NodeStateWire::Running),
            PeerState::Running
        );
    }
}
