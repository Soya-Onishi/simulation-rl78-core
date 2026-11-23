//! Command → Orchestrator successful reply bodies.

use serde::{Deserialize, Serialize};

use crate::cluster_id::ClusterId;
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

/// Coarse session status (`GET` [`super::paths::status`] or embedded in replies).
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
    /// `POST` [`super::paths::sessions`] — Command-assigned identity.
    SessionCreated { cluster_id: ClusterId },
    Status(CommandHostStatus),
    Node(NodeCommandResultDto),
    /// `GET` [`super::paths::build_status`] (`node_key` is the path param).
    BuildStatus { status: NodeProcessStatus },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_created_uses_cluster_id_object_not_path_string() {
        let reply = OrchestratorReply::SessionCreated {
            cluster_id: ClusterId::new(1, 2),
        };
        let v = serde_json::to_value(&reply).unwrap();
        assert_eq!(v["ok"], "session_created");
        assert_eq!(
            v["cluster_id"],
            serde_json::json!({"id": "1", "nano": "2"})
        );
        assert!(v["cluster_id"].as_str().is_none());
        let back: OrchestratorReply = serde_json::from_value(v).unwrap();
        assert_eq!(back, reply);
    }

    #[test]
    fn build_status_has_no_node_key_field() {
        let reply = OrchestratorReply::BuildStatus {
            status: NodeProcessStatus::Running { pid: 42 },
        };
        let v = serde_json::to_value(&reply).unwrap();
        assert_eq!(v["ok"], "build_status");
        assert!(v.get("node_key").is_none());
        assert_eq!(v["status"]["lifecycle"], "running");
        assert_eq!(v["status"]["pid"], 42);
        let back: OrchestratorReply = serde_json::from_value(v).unwrap();
        assert_eq!(back, reply);
    }
}
