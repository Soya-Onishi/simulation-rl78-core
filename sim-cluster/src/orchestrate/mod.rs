//! Multi-backend assignment and two-phase create wire types.
//!
//! These shapes are shared by `simulation-frontend` orchestration and
//! `cluster-backend` create handlers. Transports should reuse them as-is.

mod aggregate;
mod assign;
mod capacity;
mod create;
mod multi;

pub use assign::{AssignError, BackendCapacity, assign_boards, capacity_from_health};
pub use capacity::{ServerProbe, healthy_capacities, probe_servers};
pub use create::{
    ClusterCreated, ClusterMember, ClusterNode, CreateFailure, CreatePlan, Leftover,
    MulticastAllocator, create_cluster, plan_create, trim_topology_for_backend,
    validate_assignment, validate_simulation_id,
};
pub use multi::MultiBackend;

use serde::{Deserialize, Serialize};

use crate::topology::LogicalTopology;

/// `FrontendConfig.servers` entry identity: `"{ip}:{port}"`.
pub type BackendKey = String;

/// Build a [`BackendKey`] from listen coordinates.
#[must_use]
pub fn backend_key(ip: &str, port: u16) -> BackendKey {
    format!("{ip}:{port}")
}

/// One board placed on one `cluster-backend`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardAssignment {
    pub board_id: String,
    pub backend: BackendKey,
}

/// Full board → backend placement for a simulation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentTable {
    pub entries: Vec<BoardAssignment>,
}

impl AssignmentTable {
    /// Backends that own at least one board, in first-seen order.
    #[must_use]
    pub fn participating_backends(&self) -> Vec<BackendKey> {
        let mut out = Vec::new();
        for entry in &self.entries {
            if !out.contains(&entry.backend) {
                out.push(entry.backend.clone());
            }
        }
        out
    }

    /// Boards assigned to `backend`.
    #[must_use]
    pub fn boards_for(&self, backend: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|e| e.backend == backend)
            .map(|e| e.board_id.as_str())
            .collect()
    }

    /// Backend owning `board_id`, if any.
    #[must_use]
    pub fn backend_for_board(&self, board_id: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.board_id == board_id)
            .map(|e| e.backend.as_str())
    }
}

/// Advertised arbiter listen endpoints for one participating backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArbiterEndpoints {
    pub backend: BackendKey,
    pub advertise_ip: String,
    pub control_tcp_port: u16,
    pub data_udp_port: u16,
}

/// `BackendConfig` values that every participating backend must share.
///
/// Reported by `GET /internal/health`; the frontend rejects a create when the
/// participating backends disagree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSettings {
    pub time_sync_period_ms: u64,
    pub data_udp_window: u32,
    /// IPv4 multicast base address (dotted quad).
    pub multicast_base: String,
}

/// Per-simulation UDP multicast group for time sync.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MulticastGroup {
    pub ip: String,
    pub port: u16,
}

/// Phase-1 create: full topology + assignment; backend spawns local arbiter and binds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Phase1CreateRequest {
    /// Simulation id chosen by the frontend. The backend must create its
    /// simulation under exactly this id (or reject the request), so the
    /// frontend can `DELETE` it even when the reply is lost. Allowed
    /// characters: see [`validate_simulation_id`].
    pub id: String,
    pub topology: LogicalTopology,
    pub assignment: AssignmentTable,
    /// Calling backend's own [`BackendKey`] (so it knows which boards are local).
    pub local_backend: BackendKey,
}

/// Phase-1 reply with bound arbiter ports (advertise_ip from BackendConfig).
///
/// `endpoints` is `None` when this backend is the only participant: the arbiter
/// binds no peer sockets in that case (single-backend degenerate path).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase1CreateResponse {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<ArbiterEndpoints>,
}

/// Phase-2: distribute peer endpoints and multicast group; establish arbiter links.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Phase2ConnectRequest {
    pub id: String,
    /// Cluster identity shared by every participant of this simulation: the
    /// key the frontend derived each member's `id` from (`member_id`), without
    /// the per-member suffix. Time-sync multicast packets are stamped with it
    /// and accepted only when it matches, so it must be identical in every
    /// participant's request. Unlike `id` it is not an IPC (iceoryx) name.
    pub cluster_key: String,
    pub peers: Vec<ArbiterEndpoints>,
    pub multicast: MulticastGroup,
}

/// Phase-2 acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase2ConnectResponse {
    pub ok: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{BoardSpec, LogicalTopology};

    fn sample_topo() -> LogicalTopology {
        LogicalTopology {
            boards: vec![
                BoardSpec {
                    id: "a".into(),
                    kind: "rl78".into(),
                    elf: Some("base64:AA==".into()),
                    endpoints: vec![],
                },
                BoardSpec {
                    id: "b".into(),
                    kind: "plant".into(),
                    elf: Some("http://example/fw.elf".into()),
                    endpoints: vec![],
                },
            ],
            edges: vec![],
            margin_ns: 1000,
            headroom_threshold_ns: 200,
            ready_timeout_ms: 10_000,
        }
    }

    #[test]
    fn backend_key_formats_ip_port() {
        assert_eq!(backend_key("127.0.0.1", 8091), "127.0.0.1:8091");
    }

    #[test]
    fn assignment_table_helpers() {
        let table = AssignmentTable {
            entries: vec![
                BoardAssignment {
                    board_id: "a".into(),
                    backend: "10.0.0.1:8091".into(),
                },
                BoardAssignment {
                    board_id: "b".into(),
                    backend: "10.0.0.2:8091".into(),
                },
                BoardAssignment {
                    board_id: "c".into(),
                    backend: "10.0.0.1:8091".into(),
                },
            ],
        };
        assert_eq!(
            table.participating_backends(),
            vec!["10.0.0.1:8091".to_string(), "10.0.0.2:8091".to_string()]
        );
        assert_eq!(table.boards_for("10.0.0.1:8091"), vec!["a", "c"]);
        assert_eq!(table.backend_for_board("b"), Some("10.0.0.2:8091"));
        assert_eq!(table.backend_for_board("missing"), None);
    }

    #[test]
    fn phase_requests_round_trip_json() {
        let assignment = AssignmentTable {
            entries: vec![BoardAssignment {
                board_id: "a".into(),
                backend: "127.0.0.1:8091".into(),
            }],
        };
        let phase1 = Phase1CreateRequest {
            id: "sim-1".into(),
            topology: sample_topo(),
            assignment: assignment.clone(),
            local_backend: "127.0.0.1:8091".into(),
        };
        let raw = serde_json::to_string(&phase1).unwrap();
        let back: Phase1CreateRequest = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.id, "sim-1");
        assert_eq!(back.local_backend, "127.0.0.1:8091");
        assert_eq!(back.assignment, assignment);

        let phase1_resp = Phase1CreateResponse {
            id: "sim-1".into(),
            endpoints: Some(ArbiterEndpoints {
                backend: "127.0.0.1:8091".into(),
                advertise_ip: "127.0.0.1".into(),
                control_tcp_port: 9000,
                data_udp_port: 9001,
            }),
        };
        let raw = serde_json::to_string(&phase1_resp).unwrap();
        let back: Phase1CreateResponse = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, phase1_resp);

        let solo = Phase1CreateResponse {
            id: "sim-solo".into(),
            endpoints: None,
        };
        let raw = serde_json::to_string(&solo).unwrap();
        assert!(!raw.contains("endpoints"));
        let back: Phase1CreateResponse = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, solo);

        let phase2 = Phase2ConnectRequest {
            id: "sim-1-0".into(),
            cluster_key: "sim-1".into(),
            peers: vec![phase1_resp.endpoints.clone().unwrap()],
            multicast: MulticastGroup {
                ip: "239.255.0.1".into(),
                port: 47000,
            },
        };
        let raw = serde_json::to_string(&phase2).unwrap();
        let back: Phase2ConnectRequest = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.id, "sim-1-0");
        assert_eq!(back.cluster_key, "sim-1");
        assert_eq!(back.peers.len(), 1);
        assert!(back.peers[0].control_tcp_port > 0);

        let empty_peers = Phase2ConnectRequest {
            id: "sim-solo".into(),
            cluster_key: "sim-solo".into(),
            peers: vec![],
            multicast: MulticastGroup {
                ip: "239.255.0.2".into(),
                port: 47001,
            },
        };
        let raw = serde_json::to_string(&empty_peers).unwrap();
        let back: Phase2ConnectRequest = serde_json::from_str(&raw).unwrap();
        assert!(back.peers.is_empty());
    }
}
