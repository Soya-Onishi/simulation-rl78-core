//! Orchestrator → Command request bodies.
//!
//! Resource ids (`cluster_id`, `node_key`) are path params — see [`super::paths`].

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::command2node::NodeCommand;
use crate::id::HashedId;
use crate::topology::{LogicalTopology, TopologyError};

/// `POST` [`super::paths::sessions`] body.
///
/// The Orchestrator sends the whole topology to every Command host and lists
/// in `nodes` the boards (by [`crate::topology::node_key`]) this host launches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionsBody {
    pub topology: LogicalTopology,
    pub nodes: Vec<HashedId<u64>>,
}

impl SessionsBody {
    /// Valid topology, and `nodes` is a non-empty set of its boards.
    pub fn validate(&self) -> Result<(), TopologyError> {
        self.topology.validate()?;
        let boards = self.topology.boards_by_key()?;
        if self.nodes.is_empty() {
            return Err(TopologyError("nodes must not be empty".into()));
        }
        let mut seen = HashSet::new();
        for node in &self.nodes {
            if !boards.contains_key(node) {
                return Err(TopologyError(format!(
                    "node {node} is not a topology board"
                )));
            }
            if !seen.insert(node) {
                return Err(TopologyError(format!("duplicate node {node}")));
            }
        }
        Ok(())
    }

    /// Some topology board runs on another host.
    #[must_use]
    pub fn has_remote_hosts(&self) -> bool {
        self.nodes.len() < self.topology.board_count()
    }
}

/// `POST` [`super::paths::start`] body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartBody {
    /// `None` / omitted = run without a virtual-time ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ns: Option<u64>,
}

/// JSON form of a node command (variable-length fields).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeCommandDto {
    Start,
    Stop,
    Reset,
    GdbStep,
    GdbStepSync {
        until_ns: u64,
    },
    Inspect {
        payload: Vec<u8>,
    },
    Action {
        device: HashedId<u64>,
        action: HashedId<u64>,
        params: Vec<f64>,
    },
    /// Add probe ids (same semantics as wire [`NodeCommand::ProbeSubscribe`]).
    ProbeSubscribe {
        probe_ids: Vec<HashedId<u64>>,
    },
}

/// Failed conversion from JSON DTO into the iceoryx sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeCommandDtoError {
    InspectTooLarge,
    TooManyParams,
    TooManyProbes,
}

impl NodeCommandDto {
    /// Pack into the iceoryx RR sample.
    pub fn to_wire(self) -> Result<NodeCommand, NodeCommandDtoError> {
        Ok(match self {
            Self::Start => NodeCommand::Start,
            Self::Stop => NodeCommand::Stop,
            Self::Reset => NodeCommand::Reset,
            Self::GdbStep => NodeCommand::GdbStep,
            Self::GdbStepSync { until_ns } => NodeCommand::GdbStepSync { until_ns },
            Self::Inspect { payload } => {
                NodeCommand::inspect(&payload).ok_or(NodeCommandDtoError::InspectTooLarge)?
            }
            Self::Action {
                device,
                action,
                params,
            } => {
                if params.len() > sim_kernel::MAX_ACTION_PARAMS {
                    return Err(NodeCommandDtoError::TooManyParams);
                }
                let mut slots = [0f64; sim_kernel::MAX_ACTION_PARAMS];
                slots[..params.len()].copy_from_slice(&params);
                NodeCommand::Action {
                    device,
                    action,
                    param_count: params.len() as u8,
                    params: slots,
                }
            }
            Self::ProbeSubscribe { probe_ids } => NodeCommand::probe_subscribe(&probe_ids)
                .ok_or(NodeCommandDtoError::TooManyProbes)?,
        })
    }
}

/// `POST` [`super::paths::node_command`] body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeCommandBody {
    pub command: NodeCommandDto,
}

/// `POST` [`super::paths::probe_interest`] body — **add** probe ids for aggregation
/// toward Orchestrator (same add semantics as wire `NodeCommand::ProbeSubscribe`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeInterestBody {
    pub probe_ids: Vec<HashedId<u64>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command2node::MAX_NODE_PROBE_IDS;

    fn sessions_body(nodes: &[&str]) -> SessionsBody {
        let topology = serde_json::from_value(serde_json::json!({
            "boards": [
                {"id": "a", "kind": "rl78", "endpoints": []},
                {"id": "b", "kind": "rl78", "endpoints": []},
            ],
            "edges": [],
            "margin_ns": 1,
        }))
        .unwrap();
        SessionsBody {
            topology,
            nodes: nodes
                .iter()
                .map(|id| crate::topology::node_key(id))
                .collect(),
        }
    }

    #[test]
    fn sessions_body_nodes_must_be_unique_topology_boards() {
        assert_eq!(sessions_body(&["a"]).validate(), Ok(()));
        assert!(sessions_body(&["a"]).has_remote_hosts());
        assert!(!sessions_body(&["a", "b"]).has_remote_hosts());
        for (nodes, expected) in [
            (&[][..], "nodes must not be empty"),
            (&["c"][..], "is not a topology board"),
            (&["a", "a"][..], "duplicate node"),
        ] {
            let message = sessions_body(nodes).validate().unwrap_err().0;
            assert!(
                message.contains(expected),
                "{expected:?} not in {message:?}"
            );
        }
    }

    #[test]
    fn sessions_body_rejects_invalid_topology() {
        let mut body = sessions_body(&["a"]);
        body.topology.boards[1].id = "a".into();
        assert!(
            body.validate()
                .unwrap_err()
                .0
                .contains("duplicate board id")
        );
    }

    #[test]
    fn start_body_omits_none_until_ns() {
        let body = StartBody { until_ns: None };
        assert_eq!(serde_json::to_value(&body).unwrap(), serde_json::json!({}));
        let back: StartBody = serde_json::from_str("{}").unwrap();
        assert_eq!(back.until_ns, None);
    }

    #[test]
    fn probe_interest_round_trip() {
        let body = ProbeInterestBody {
            probe_ids: vec![HashedId::new(1), HashedId::new(2)],
        };
        let s = serde_json::to_string(&body).unwrap();
        let back: ProbeInterestBody = serde_json::from_str(&s).unwrap();
        assert_eq!(back, body);
    }

    #[test]
    fn dto_to_wire_probe_subscribe() {
        let dto = NodeCommandDto::ProbeSubscribe {
            probe_ids: vec![HashedId::new(1), HashedId::new(2)],
        };
        let wire = dto.to_wire().unwrap();
        match wire {
            NodeCommand::ProbeSubscribe { count, probe_ids } => {
                assert_eq!(count, 2);
                assert_eq!(probe_ids[0], HashedId::new(1));
                assert_eq!(probe_ids[1], HashedId::new(2));
            }
            _ => panic!("expected ProbeSubscribe"),
        }
    }

    #[test]
    fn dto_rejects_too_many_probes() {
        let ids = (0..=MAX_NODE_PROBE_IDS as u64)
            .map(HashedId::new)
            .collect::<Vec<_>>();
        let dto = NodeCommandDto::ProbeSubscribe { probe_ids: ids };
        assert_eq!(dto.to_wire(), Err(NodeCommandDtoError::TooManyProbes));
    }

    #[test]
    fn action_ids_serde_as_hex() {
        let dto = NodeCommandDto::Action {
            device: HashedId::new(0xabcdu64),
            action: HashedId::new(0x10u64),
            params: vec![1.5],
        };
        let v = serde_json::to_value(&dto).unwrap();
        assert_eq!(v["device"], "000000000000abcd");
        assert_eq!(v["action"], "0000000000000010");
        let back: NodeCommandDto = serde_json::from_value(v).unwrap();
        let wire = back.to_wire().unwrap();
        match wire {
            NodeCommand::Action {
                device,
                action,
                param_count,
                params,
            } => {
                assert_eq!(device, HashedId::new(0xabcd));
                assert_eq!(action, HashedId::new(0x10));
                assert_eq!(param_count, 1);
                assert_eq!(params[0], 1.5);
            }
            _ => panic!("expected Action"),
        }
    }
}
