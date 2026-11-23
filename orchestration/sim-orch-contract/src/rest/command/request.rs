//! Orchestrator → Command request bodies.

use serde::{Deserialize, Serialize};

use crate::rest::node_dto::NodeCommandDto;

/// JSON body for Orchestrator → Command `POST` routes.
///
/// Pairing with paths is documented on each variant. `GET` routes take no body
/// (see [`super::paths::PATH_STATUS`], [`super::paths::PATH_BUILD_STATUS`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OrchestratorRequest {
    /// `POST` [`super::paths::PATH_SESSION`]
    EnsureSession { cluster_key: String },
    /// `POST` [`super::paths::PATH_START`]
    Start { until_ns: Option<u64> },
    /// `POST` [`super::paths::PATH_STOP`]
    Stop,
    /// `POST` [`super::paths::PATH_RESET`]
    Reset,
    /// `POST` [`super::paths::PATH_NODE_COMMAND`] — Command fans out via iceoryx RR.
    NodeCommand {
        node_key: String,
        command: NodeCommandDto,
    },
    /// `POST` [`super::paths::PATH_PROBE_INTEREST`] — **add** probe ids for aggregation
    /// toward Orchestrator (same add semantics as wire `NodeCommand::ProbeSubscribe`).
    ProbeInterest {
        node_key: String,
        probe_ids: Vec<u64>,
    },
    /// `POST` [`super::paths::PATH_BUILD_ENSURE`] — Build thread stub (spawn / ensure alive).
    BuildEnsure { node_key: String },
    /// `POST` [`super::paths::PATH_BUILD_RESTART`]
    BuildRestart { node_key: String },
    /// `POST` [`super::paths::PATH_BUILD_TERMINATE`]
    BuildTerminate { node_key: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_json_round_trip() {
        let req = OrchestratorRequest::EnsureSession {
            cluster_key: "abc".into(),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["op"], "ensure_session");
        let back: OrchestratorRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
    }
}
