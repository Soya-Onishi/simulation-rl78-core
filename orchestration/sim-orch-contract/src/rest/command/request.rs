//! Orchestrator → Command request bodies.

use serde::{Deserialize, Serialize};

use crate::cluster_id::ClusterId;
use crate::hashed_id::HashedId;
use crate::rest::node_dto::NodeCommandDto;

/// JSON body for Orchestrator → Command `POST` routes.
///
/// Pairing with paths is documented on each variant. `GET` routes take no body
/// (see [`super::paths::PATH_STATUS`], [`super::paths::PATH_BUILD_STATUS`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OrchestratorRequest {
    /// `POST` [`super::paths::PATH_SESSION`]
    EnsureSession { cluster_id: ClusterId },
    /// `POST` [`super::paths::PATH_START`]
    Start { until_ns: Option<u64> },
    /// `POST` [`super::paths::PATH_STOP`]
    Stop,
    /// `POST` [`super::paths::PATH_RESET`]
    Reset,
    /// `POST` [`super::paths::PATH_NODE_COMMAND`] — Command fans out via iceoryx RR.
    NodeCommand {
        node_key: HashedId<u64>,
        command: NodeCommandDto,
    },
    /// `POST` [`super::paths::PATH_PROBE_INTEREST`] — **add** probe ids for aggregation
    /// toward Orchestrator (same add semantics as wire `NodeCommand::ProbeSubscribe`).
    ProbeInterest {
        node_key: HashedId<u64>,
        probe_ids: Vec<HashedId<u64>>,
    },
    /// `POST` [`super::paths::PATH_BUILD_ENSURE`] — Build thread stub (spawn / ensure alive).
    BuildEnsure { node_key: HashedId<u64> },
    /// `POST` [`super::paths::PATH_BUILD_RESTART`]
    BuildRestart { node_key: HashedId<u64> },
    /// `POST` [`super::paths::PATH_BUILD_TERMINATE`]
    BuildTerminate { node_key: HashedId<u64> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_json_round_trip() {
        let req = OrchestratorRequest::EnsureSession {
            cluster_id: ClusterId::new(1, 2),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"op\":\"ensure_session\""));
        assert!(s.contains("\"cluster_id\":{\"id\":\"1\",\"nano\":\"2\"}"));
        let back: OrchestratorRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back, req);
    }
}
