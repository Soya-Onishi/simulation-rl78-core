//! JSON DTOs bridging Orchestrator/Command REST to iceoryx [`NodeCommand`].

use serde::{Deserialize, Serialize};

use crate::hashed_id::HashedId;
use crate::node_cmd::{INSPECT_CAP, NodeCommand, NodeCommandError, NodeCommandResult};

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

/// JSON form of a node command result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeCommandResultDto {
    Success,
    InspectResult { payload: Vec<u8> },
    Error { code: NodeCommandErrorDto },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeCommandErrorDto {
    Unavailable,
    ImplFault,
    NotFoundDevice,
    NotFoundAction,
    BadParam,
    Busy,
}

impl From<NodeCommandError> for NodeCommandErrorDto {
    fn from(value: NodeCommandError) -> Self {
        match value {
            NodeCommandError::Unavailable => Self::Unavailable,
            NodeCommandError::ImplFault => Self::ImplFault,
            NodeCommandError::NotFoundDevice => Self::NotFoundDevice,
            NodeCommandError::NotFoundAction => Self::NotFoundAction,
            NodeCommandError::BadParam => Self::BadParam,
            NodeCommandError::Busy => Self::Busy,
        }
    }
}

impl From<NodeCommandResult> for NodeCommandResultDto {
    fn from(value: NodeCommandResult) -> Self {
        match value {
            NodeCommandResult::Success => Self::Success,
            NodeCommandResult::InspectResult { len, payload } => {
                let n = usize::from(len).min(INSPECT_CAP);
                Self::InspectResult {
                    payload: payload[..n].to_vec(),
                }
            }
            NodeCommandResult::Error(e) => Self::Error { code: e.into() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_NODE_PROBE_IDS;

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
