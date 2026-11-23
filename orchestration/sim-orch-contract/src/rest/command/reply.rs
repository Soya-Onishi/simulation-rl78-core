//! Command → Orchestrator successful reply bodies.

use serde::{Deserialize, Serialize};

use crate::rest::node_dto::NodeCommandResultDto;

/// Node process lifecycle as observed by the Build thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "lifecycle", rename_all = "snake_case")]
pub enum NodeProcessStatus {
    NotStarted,
    Running { pid: u32 },
    Exited { code: Option<i32> },
    Unknown { detail: String },
}

/// Coarse host / command status (`GET` [`super::paths::PATH_STATUS`] or embedded in replies).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CommandHostStatus {
    Idle,
    Running,
    Stopping,
    Fault { message: String },
}

/// Successful Command reply body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ok", rename_all = "snake_case")]
pub enum OrchestratorReply {
    Ack,
    Status(CommandHostStatus),
    Node(NodeCommandResultDto),
    /// `GET` [`super::paths::PATH_BUILD_STATUS`] body (`node_key` matches the path param).
    BuildStatus {
        node_key: String,
        status: NodeProcessStatus,
    },
}
