//! Orchestrator → Command request bodies.
//!
//! Resource ids (`cluster_id`, `node_key`) are path params — see [`super::paths`].

use serde::{Deserialize, Serialize};

use crate::id::HashedId;
use crate::command2node::NodeCommand;

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
        device: u64,
        action: u64,
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
}
