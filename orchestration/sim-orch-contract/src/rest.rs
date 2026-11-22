//! Orchestrator ↔ Command REST contract (final transport; same-binary first).
//!
//! Stable HTTP surface so in-process adapters and a later process split share
//! one protocol (no throwaway mpsc). Bodies are JSON of the types below.
//! iceoryx RR uses [`crate::node_cmd::NodeCommand`]; this module holds the JSON
//! DTOs and conversion into the wire sample.

use serde::{Deserialize, Serialize};

use crate::node_cmd::{INSPECT_CAP, NodeCommand, NodeCommandError, NodeCommandResult};

/// URL prefix for Command host API (Orchestrator is the client).
pub const COMMAND_API_PREFIX: &str = "/v1/command";

/// `POST` — ensure / attach session.
pub const PATH_SESSION: &str = "/v1/command/session";
/// `POST`
pub const PATH_START: &str = "/v1/command/start";
/// `POST`
pub const PATH_STOP: &str = "/v1/command/stop";
/// `POST`
pub const PATH_RESET: &str = "/v1/command/reset";
/// `POST`
pub const PATH_NODE_COMMAND: &str = "/v1/command/node";
/// `POST`
pub const PATH_PROBE_INTEREST: &str = "/v1/command/probe-interest";
/// `GET`
pub const PATH_STATUS: &str = "/v1/command/status";
/// `POST` — Build/Launch stub: spawn or ensure a node process.
pub const PATH_BUILD_ENSURE: &str = "/v1/command/build/ensure";
/// `POST` — restart a node process.
pub const PATH_BUILD_RESTART: &str = "/v1/command/build/restart";
/// `POST` — terminate a node process.
pub const PATH_BUILD_TERMINATE: &str = "/v1/command/build/terminate";
/// `GET` — node process lifecycle status (Build thread).
pub const PATH_BUILD_STATUS: &str = "/v1/command/build/status";

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
        probe_ids: Vec<u64>,
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

/// Orchestrator → Command request body (also documents method + path pairing).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OrchToCommand {
    /// `POST` [`PATH_SESSION`]
    EnsureSession { cluster_key: String },
    /// `POST` [`PATH_START`]
    Start { until_ns: Option<u64> },
    /// `POST` [`PATH_STOP`]
    Stop,
    /// `POST` [`PATH_RESET`]
    Reset,
    /// `POST` [`PATH_NODE_COMMAND`] — Command fans out via iceoryx RR.
    NodeCommand {
        node_key: String,
        command: NodeCommandDto,
    },
    /// `POST` [`PATH_PROBE_INTEREST`] — **add** probe ids for aggregation
    /// toward Orchestrator (same add semantics as [`NodeCommand::ProbeSubscribe`]).
    ProbeInterest {
        node_key: String,
        probe_ids: Vec<u64>,
    },
    /// `POST` [`PATH_BUILD_ENSURE`] — Build thread stub (spawn / ensure alive).
    BuildEnsure { node_key: String },
    /// `POST` [`PATH_BUILD_RESTART`]
    BuildRestart { node_key: String },
    /// `POST` [`PATH_BUILD_TERMINATE`]
    BuildTerminate { node_key: String },
}

/// Node process lifecycle as observed by the Build thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "lifecycle", rename_all = "snake_case")]
pub enum NodeProcessStatus {
    NotStarted,
    Running { pid: u32 },
    Exited { code: Option<i32> },
    Unknown { detail: String },
}

/// Successful Command reply body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ok", rename_all = "snake_case")]
pub enum OrchToCommandReply {
    Ack,
    Status(CommandHostStatus),
    Node(NodeCommandResultDto),
    /// `GET` [`PATH_BUILD_STATUS`] body.
    BuildStatus {
        node_key: String,
        status: NodeProcessStatus,
    },
}

/// Error body; HTTP status should be 4xx/5xx as noted on [`RestErrorCode`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestErrorBody {
    pub code: RestErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestErrorCode {
    /// HTTP 400
    BadRequest,
    /// HTTP 404
    NotFound,
    /// HTTP 409
    Conflict,
    /// HTTP 503
    Unavailable,
    /// HTTP 500
    Internal,
}

/// Coarse host / command status (`GET` [`PATH_STATUS`] or embedded in replies).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CommandHostStatus {
    Idle,
    Running,
    Stopping,
    Fault { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_NODE_PROBE_IDS;

    #[test]
    fn session_json_round_trip() {
        let req = OrchToCommand::EnsureSession {
            cluster_key: "abc".into(),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "ensure_session");
        let back: OrchToCommand = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn dto_to_wire_probe_subscribe() {
        let dto = NodeCommandDto::ProbeSubscribe {
            probe_ids: vec![1, 2],
        };
        let wire = dto.to_wire().unwrap();
        match wire {
            NodeCommand::ProbeSubscribe { count, probe_ids } => {
                assert_eq!(count, 2);
                assert_eq!(probe_ids[0], 1);
                assert_eq!(probe_ids[1], 2);
            }
            _ => panic!("expected ProbeSubscribe"),
        }
    }

    #[test]
    fn dto_rejects_too_many_probes() {
        let ids = (0..=MAX_NODE_PROBE_IDS as u64).collect::<Vec<_>>();
        let dto = NodeCommandDto::ProbeSubscribe { probe_ids: ids };
        assert_eq!(dto.to_wire(), Err(NodeCommandDtoError::TooManyProbes));
    }
}
