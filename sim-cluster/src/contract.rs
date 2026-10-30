//! Shared HTTP contract types for client↔web and web↔sim.
//!
//! These shapes are the stable wire vocabulary. Transports (axum handlers,
//! future HTTP clients to remote sim-servers) should reuse them.

use serde::{Deserialize, Serialize};

use crate::elf_source;
use crate::host::SessionNode;
use crate::topology::LogicalTopology;

/// Body for `POST /api/simulation`.
#[derive(Debug, Deserialize)]
pub struct CreateSimulationRequest {
    pub topology: LogicalTopology,
}

/// Topology accepted by create. Local paths and `file://` are rejected.
///
/// HTTP and the sim worker both call this so the check stays valid when those
/// sides become separate processes.
pub fn prepare_create_topology(topology: LogicalTopology) -> Result<LogicalTopology, ApiError> {
    topology
        .validate()
        .map_err(|err| ApiError::bad_request("invalid_topology", err.to_string()))?;
    for board in &topology.boards {
        if let Some(elf) = &board.elf {
            elf_source::check_http_create_spec(elf).map_err(|err| {
                ApiError::bad_request("invalid_topology", format!("board {}: {err}", board.id))
            })?;
        }
    }
    Ok(topology)
}

/// Body for `201` create responses.
#[derive(Debug, Serialize)]
pub struct CreateSimulationResponse {
    pub id: String,
}

/// Snapshot for `GET /api/simulation/{id}` and SSE `status` events.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusEvent {
    pub id: String,
    pub state: String,
    pub virtual_time_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub nodes: Vec<SessionNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gdb: Option<GdbSessionInfo>,
}

/// Body for `POST /api/simulation/{id}/gdb`.
#[derive(Debug, Deserialize)]
pub struct AttachGdbRequest {
    pub board_id: String,
}

/// Live gdbserver slot advertised by cluster-server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdbSessionInfo {
    pub board_id: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub ssh_command: String,
    pub connected: bool,
}

/// Body for `POST /api/simulation/{id}/start`. Empty / omitted `until_ns` is unlimited.
#[derive(Debug, Default, Deserialize)]
pub struct StartSimulationRequest {
    pub until_ns: Option<u64>,
}

impl StartSimulationRequest {
    /// Parse a start body. Empty bytes are unlimited.
    pub fn from_body(body: &[u8]) -> Result<Self, ApiError> {
        let trimmed = body.trim_ascii();
        if trimmed.is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_slice(trimmed)
            .map_err(|_| ApiError::bad_request("bad_param", "invalid start body"))
    }
}

/// SSE `log` event payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEvent {
    pub seq: u64,
    pub level: String,
    pub origin: String,
    pub board: Option<String>,
    pub text: String,
}

/// One parameter in an action catalog row. Hashes are not part of the JSON contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdvertisedActionParam {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<f64>,
}

/// One action a client can invoke. Identity fields are strings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdvertisedAction {
    pub board_id: String,
    pub device: String,
    pub action: String,
    pub description: String,
    pub params: Vec<AdvertisedActionParam>,
}

/// `GET /api/simulation/{id}/actions` body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionList {
    pub actions: Vec<AdvertisedAction>,
}

/// `POST /api/simulation/{id}/actions` body.
#[derive(Clone, Debug, Deserialize)]
pub struct InvokeActionRequest {
    pub board_id: String,
    pub device: String,
    pub action: String,
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, f64>,
}

/// One row of `GET /api/simulation/{id}/probes/samples`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeSampleJson {
    pub virtual_time_ns: u64,
    pub board: String,
    pub probe: String,
    pub kind: String,
    pub value: serde_json::Value,
}

/// Body for `GET /api/simulation/{id}/probes/samples`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeSamplesResponse {
    pub truncated: bool,
    pub samples: Vec<ProbeSampleJson>,
}

/// Uniform API error envelope: `{"error":{"code","message"}}`.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

/// Failure returned through the HTTP layer and backend trait.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    #[must_use]
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(400, code, message)
    }

    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "not_found", message)
    }

    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, "conflict", message)
    }

    #[must_use]
    pub fn viewer_busy() -> Self {
        Self::new(409, "viewer_busy", "simulation is busy")
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "internal", message)
    }

    #[must_use]
    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            error: ErrorDetail {
                code: self.code.to_string(),
                message: self.message.clone(),
            },
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
mod tests {
    use super::{
        ProbeSampleJson, ProbeSamplesResponse, StartSimulationRequest, prepare_create_topology,
    };
    use crate::topology::LogicalTopology;

    fn one_board(elf: Option<&str>) -> LogicalTopology {
        LogicalTopology {
            boards: vec![crate::topology::BoardSpec {
                id: "a".into(),
                kind: "rl78".into(),
                elf: elf.map(str::to_string),
                endpoints: vec![],
            }],
            edges: vec![],
            margin_ns: 1000,
            headroom_threshold_ns: 200,
            ready_timeout_ms: 10_000,
        }
    }

    #[test]
    fn prepare_create_topology_requires_boards() {
        let err = prepare_create_topology(LogicalTopology {
            boards: vec![],
            edges: vec![],
            margin_ns: 1000,
            headroom_threshold_ns: 200,
            ready_timeout_ms: 10_000,
        })
        .unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.code, "invalid_topology");
    }

    #[test]
    fn prepare_create_topology_rejects_local_elf_paths() {
        for elf in ["a.elf", "/tmp/a.elf", "file:///tmp/a.elf"] {
            let err = prepare_create_topology(one_board(Some(elf))).unwrap_err();
            assert_eq!(err.status, 400);
            assert_eq!(err.code, "invalid_topology");
            assert!(
                err.message.contains("http(s)") || err.message.contains("base64"),
                "unexpected message: {}",
                err.message
            );
        }
    }

    #[test]
    fn prepare_create_topology_accepts_remote_and_inline_elf() {
        for elf in ["https://example.test/fw.elf", "base64:AQI="] {
            let topo = prepare_create_topology(one_board(Some(elf))).expect(elf);
            assert_eq!(topo.boards[0].elf.as_deref(), Some(elf));
        }
        assert!(prepare_create_topology(one_board(None)).is_ok());
    }

    #[test]
    fn samples_body_uses_typed_values() {
        let body = ProbeSamplesResponse {
            truncated: false,
            samples: vec![
                ProbeSampleJson {
                    virtual_time_ns: 1,
                    board: "a".into(),
                    probe: "p01".into(),
                    kind: "digital".into(),
                    value: serde_json::json!("High"),
                },
                ProbeSampleJson {
                    virtual_time_ns: 2,
                    board: "a".into(),
                    probe: "uart0".into(),
                    kind: "uart".into(),
                    value: serde_json::json!(65),
                },
            ],
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["samples"][0]["value"], "High");
        assert_eq!(json["samples"][1]["value"], 65);
    }

    #[test]
    fn start_body_empty_is_unlimited() {
        assert_eq!(
            StartSimulationRequest::from_body(b"").unwrap().until_ns,
            None
        );
        assert_eq!(
            StartSimulationRequest::from_body(b" \n\t")
                .unwrap()
                .until_ns,
            None
        );
        assert_eq!(
            StartSimulationRequest::from_body(b"{}").unwrap().until_ns,
            None
        );
    }

    #[test]
    fn start_body_reads_until_ns() {
        let parsed = StartSimulationRequest::from_body(b"{\"until_ns\":1000}").unwrap();
        assert_eq!(parsed.until_ns, Some(1000));
    }

    #[test]
    fn start_body_rejects_bad_json() {
        assert!(StartSimulationRequest::from_body(b"not-json").is_err());
        assert!(StartSimulationRequest::from_body(b"{\"until_ns\":\"x\"}").is_err());
    }
}
