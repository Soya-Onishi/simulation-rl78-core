//! Shared HTTP contract types for client↔web and web↔sim.
//!
//! These shapes are the stable wire vocabulary. Transports (axum handlers,
//! future HTTP clients to remote sim-servers) should reuse them.

use serde::{Deserialize, Serialize};

use crate::elf_source;
use crate::host::SessionNode;
use crate::topology::LogicalTopology;

/// Body for `POST /api/simulation` and `POST /internal/simulations`.
#[derive(Debug, Serialize, Deserialize)]
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
    crate::topology::build_topology_hashes(&topology)
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
#[derive(Debug, Serialize, Deserialize)]
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
}

/// Body for `GET /api/simulation/{id}/gdb/ssh`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdbSshCommand {
    pub command: String,
}

/// Body for `POST /api/simulation/{id}/gdb`.
#[derive(Debug, Deserialize)]
pub struct AttachGdbRequest {
    pub board_id: crate::ipc::HashedId<u64>,
}

/// Live gdbserver slot advertised by the web layer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GdbSessionInfo {
    pub board_id: crate::ipc::HashedId<u64>,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub connected: bool,
}

/// Body for `POST /api/simulation/{id}/start`. Empty / omitted `until_ns` is unlimited.
#[derive(Debug, Default, Serialize, Deserialize)]
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
    pub board: Option<crate::ipc::HashedId<u64>>,
    pub text: String,
}

/// One parameter in an action catalog row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdvertisedActionParam {
    /// `id_hash` of the parameter name (serde: 16-digit hex).
    pub name: crate::ipc::HashedId<u64>,
    /// Human-readable parameter name.
    pub name_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<f64>,
}

/// One action a client can invoke. `board_id` is the public 16-digit hex id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdvertisedAction {
    pub board_id: crate::ipc::HashedId<u64>,
    /// Human-readable board id from the topology definition.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub board_label: String,
    /// `id_hash` of the device name (serde: 16-digit hex).
    pub device: crate::ipc::HashedId<u64>,
    /// Human-readable device name.
    pub device_label: String,
    /// `id_hash` of the action name (serde: 16-digit hex).
    pub action: crate::ipc::HashedId<u64>,
    /// Human-readable action name.
    pub action_label: String,
    pub description: String,
    pub params: Vec<AdvertisedActionParam>,
}

/// `GET /api/simulation/{id}/actions` body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionList {
    pub actions: Vec<AdvertisedAction>,
}

/// `POST /api/simulation/{id}/actions` body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvokeActionRequest {
    pub board_id: crate::ipc::HashedId<u64>,
    pub device: crate::ipc::HashedId<u64>,
    pub action: crate::ipc::HashedId<u64>,
    /// Keys are `id_hash` of parameter names (serde: 16-digit hex).
    #[serde(default)]
    pub params: std::collections::BTreeMap<crate::ipc::HashedId<u64>, f64>,
}

/// One row of `GET /api/simulation/{id}/probes/samples`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeSampleJson {
    pub virtual_time_ns: u64,
    pub board: crate::ipc::HashedId<u64>,
    /// `probe_name_hash` of the probe name (serde: 16-digit hex).
    pub probe: crate::ipc::HashedId<u64>,
    /// Human-readable probe name.
    pub probe_label: String,
    pub kind: String,
    pub value: serde_json::Value,
}

/// Body for `GET /api/simulation/{id}/probes/samples`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeSamplesResponse {
    pub truncated: bool,
    pub samples: Vec<ProbeSampleJson>,
}

/// One endpoint in `GET /api/simulation/{id}/topology`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyEndpointView {
    /// `xxh3_64` of the endpoint name (serde: 16-digit hex).
    pub name: crate::ipc::HashedId<u64>,
    /// Human-readable endpoint name from the topology definition.
    pub label: String,
    pub direction: crate::topology::EndpointDirection,
    pub payload: crate::topology::PayloadKind,
}

/// One hop in a topology edge view (`path` / `port` are public hex ids).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyPortRefView {
    pub path: crate::ipc::HashedId<u64>,
    pub path_label: String,
    pub port: crate::ipc::HashedId<u64>,
    pub port_label: String,
}

/// One directed edge in `GET /api/simulation/{id}/topology`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyEdgeView {
    pub from: TopologyPortRefView,
    pub to: TopologyPortRefView,
    pub route: Vec<TopologyPortRefView>,
    pub payload: crate::topology::PayloadKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uart_ring_len: Option<u32>,
}

/// One board in `GET /api/simulation/{id}/topology` (no `elf`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyBoardView {
    /// `xxh3_64` of `boards[].id` (serde: 16-digit hex).
    pub id: crate::ipc::HashedId<u64>,
    /// Human-readable `boards[].id` from the topology definition.
    pub label: String,
    pub kind: String,
    pub endpoints: Vec<TopologyEndpointView>,
}

/// Wiring-diagram topology without firmware blobs or timing knobs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyView {
    pub boards: Vec<TopologyBoardView>,
    pub edges: Vec<TopologyEdgeView>,
}

impl TopologyView {
    /// Drop `elf` and margin fields; emit public hex ids with display labels.
    #[must_use]
    pub fn from_logical(topo: &LogicalTopology) -> Self {
        use crate::ipc::{board_id_hash, port_id_hash};
        use crate::topology::PortRef;

        let port_ref = |r: &PortRef| TopologyPortRefView {
            path: board_id_hash(&r.path),
            path_label: r.path.clone(),
            port: port_id_hash(&r.port),
            port_label: r.port.clone(),
        };

        Self {
            boards: topo
                .boards
                .iter()
                .map(|board| TopologyBoardView {
                    id: board_id_hash(&board.id),
                    label: board.id.clone(),
                    kind: board.kind.clone(),
                    endpoints: board
                        .endpoints
                        .iter()
                        .map(|ep| TopologyEndpointView {
                            name: port_id_hash(&ep.name),
                            label: ep.name.clone(),
                            direction: ep.direction,
                            payload: ep.payload,
                        })
                        .collect(),
                })
                .collect(),
            edges: topo
                .edges
                .iter()
                .map(|edge| TopologyEdgeView {
                    from: port_ref(&edge.from),
                    to: port_ref(&edge.to),
                    route: edge.route.iter().map(port_ref).collect(),
                    payload: edge.payload,
                    uart_ring_len: edge.uart_ring_len,
                })
                .collect(),
        }
    }
}

/// Uniform API error envelope: `{"error":{"code","message"}}`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize, Deserialize)]
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

    /// Rebuild from an internal HTTP error response.
    #[must_use]
    pub fn from_remote(status: u16, body: &ErrorBody) -> Self {
        let code = match body.error.code.as_str() {
            "not_found" => "not_found",
            "conflict" => "conflict",
            "viewer_busy" => "viewer_busy",
            "internal" => "internal",
            "invalid_topology" => "invalid_topology",
            "bad_param" => "bad_param",
            other => Box::leak(other.to_string().into_boxed_str()),
        };
        Self {
            status,
            code,
            message: body.error.message.clone(),
        }
    }
}

/// Hex id carried as a plain string in HTTP query / path parameters.
///
/// API query and path parameters use this type instead of deserializing
/// [`crate::ipc::HashedId`] directly so invalid values can return
/// [`ApiError`] with code `bad_param`, rather than axum's default
/// rejection response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HashedIdString(String);

impl HashedIdString {
    /// Borrow the raw parameter text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True when the client omitted a value or sent an empty string.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Parse as a public 16-digit lowercase hex [`crate::ipc::HashedId`].
    pub fn parse_id(&self, field: &str) -> Result<crate::ipc::HashedId<u64>, ApiError> {
        self.0.parse().map_err(|_| {
            ApiError::bad_request(
                "bad_param",
                format!("{field} must be a 16-digit lowercase hex string"),
            )
        })
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
        AdvertisedAction, AdvertisedActionParam, HashedIdString, InvokeActionRequest,
        ProbeSampleJson, ProbeSamplesResponse, StartSimulationRequest, TopologyView,
        prepare_create_topology,
    };
    use crate::topology::LogicalTopology;
    use sim_kernel::id_hash;

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
    fn topology_view_emits_hex_ids_with_labels() {
        let view = TopologyView::from_logical(&one_board(None));
        assert_eq!(view.boards[0].label, "a");
        assert_eq!(view.boards[0].id, crate::ipc::board_id_hash("a"));
        assert_eq!(view.boards[0].id.to_string().len(), 16);
    }

    #[test]
    fn samples_body_uses_typed_values() {
        let body = ProbeSamplesResponse {
            truncated: false,
            samples: vec![
                ProbeSampleJson {
                    virtual_time_ns: 1,
                    board: crate::ipc::board_id_hash("a"),
                    probe: crate::ipc::probe_name_hash("p01"),
                    probe_label: "p01".into(),
                    kind: "digital".into(),
                    value: serde_json::json!("High"),
                },
                ProbeSampleJson {
                    virtual_time_ns: 2,
                    board: crate::ipc::board_id_hash("a"),
                    probe: crate::ipc::probe_name_hash("uart0"),
                    probe_label: "uart0".into(),
                    kind: "uart".into(),
                    value: serde_json::json!(65),
                },
            ],
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["samples"][0]["value"], "High");
        assert_eq!(json["samples"][1]["value"], 65);
        assert_eq!(
            json["samples"][0]["probe"],
            crate::ipc::probe_name_hash("p01").to_string()
        );
        assert_eq!(json["samples"][0]["probe_label"], "p01");
    }

    #[test]
    fn hashed_id_string_rejects_bad_hex_as_bad_param() {
        let bad = HashedIdString("ABCDEF0123456789".into());
        let err = bad.parse_id("board").unwrap_err();
        assert_eq!(err.code, "bad_param");
        assert!(
            HashedIdString("0x0123456789abcdef".into())
                .parse_id("board")
                .is_err()
        );
        let ok = crate::ipc::board_id_hash("mcu-a");
        assert_eq!(
            HashedIdString(ok.to_string()).parse_id("board").unwrap(),
            ok
        );
    }

    #[test]
    fn invoke_request_wire_uses_hex_ids_for_device_action_params() {
        let device = crate::ipc::HashedId::new(id_hash("sw1"));
        let action = crate::ipc::HashedId::new(id_hash("hold"));
        let param = crate::ipc::HashedId::new(id_hash("milliseconds"));
        let board = crate::ipc::board_id_hash("mcu-a");
        let json = serde_json::json!({
            "board_id": board.to_string(),
            "device": device.to_string(),
            "action": action.to_string(),
            "params": { param.to_string(): 12.0 },
        });
        let req: InvokeActionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.board_id, board);
        assert_eq!(req.device, device);
        assert_eq!(req.action, action);
        assert_eq!(req.params.get(&param), Some(&12.0));
    }

    #[test]
    fn advertised_action_wire_keeps_hex_ids_and_labels() {
        let body = AdvertisedAction {
            board_id: crate::ipc::board_id_hash("mcu-a"),
            board_label: "mcu-a".into(),
            device: crate::ipc::HashedId::new(id_hash("sw1")),
            device_label: "sw1".into(),
            action: crate::ipc::HashedId::new(id_hash("hold")),
            action_label: "hold".into(),
            description: "press".into(),
            params: vec![AdvertisedActionParam {
                name: crate::ipc::HashedId::new(id_hash("milliseconds")),
                name_label: "milliseconds".into(),
                unit: Some("ms".into()),
                description: None,
                min: Some(1.0),
                max: None,
                default: None,
            }],
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["device"], body.device.to_string());
        assert_eq!(json["device_label"], "sw1");
        assert_eq!(json["action"], body.action.to_string());
        assert_eq!(json["action_label"], "hold");
        assert_eq!(json["params"][0]["name"], body.params[0].name.to_string());
        assert_eq!(json["params"][0]["name_label"], "milliseconds");
    }

    #[test]
    fn advertised_action_param_deserializes_omitted_optionals() {
        let name = crate::ipc::HashedId::new(id_hash("level"));
        let parsed: AdvertisedActionParam = serde_json::from_value(serde_json::json!({
            "name": name.to_string(),
            "name_label": "level",
        }))
        .unwrap();
        assert_eq!(parsed.name, name);
        assert_eq!(parsed.name_label, "level");
        assert_eq!(parsed.unit, None);
        assert_eq!(parsed.description, None);
        assert_eq!(parsed.min, None);
        assert_eq!(parsed.max, None);
        assert_eq!(parsed.default, None);
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
