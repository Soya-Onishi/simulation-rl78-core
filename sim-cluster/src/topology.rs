//! Logical topology JSON schema (runtime handoff from the Python DSL).
//!
//! This is not a checked-in product config. The singleton [`crate::server`]
//! executes Python, validates the result against these types, then starts the
//! arbiter.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Default Ready-barrier timeout when omitted from topology JSON (milliseconds).
pub const DEFAULT_READY_TIMEOUT_MS: u64 = 10_000;

/// Default UART ring capacity (frames) when an edge omits `uart_ring_len`.
pub const DEFAULT_UART_RING_LEN: u32 = 64;

/// Payload carried on a board-edge endpoint / directed data edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadKind {
    Uart,
    Digital,
}

/// Direction of an external endpoint relative to the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointDirection {
    In,
    Out,
}

/// One named board-edge endpoint in the logical topology.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointSpec {
    pub name: String,
    pub direction: EndpointDirection,
    pub payload: PayloadKind,
}

/// One simulation board (one OS process / one `Machine` in the node binary).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSpec {
    pub id: String,
    /// Board kind string (e.g. `"rl78"`). Validated later by the node.
    pub kind: String,
    /// Optional guest firmware path for the node to load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elf: Option<String>,
    pub endpoints: Vec<EndpointSpec>,
}

/// Directed data-plane edge between two endpoints (one SHM segment later).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectedEdge {
    pub from_board: String,
    pub from_endpoint: String,
    pub to_board: String,
    pub to_endpoint: String,
    pub payload: PayloadKind,
    /// UART ring length; ignored for non-UART payloads. Omitted → default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uart_ring_len: Option<u32>,
}

/// Logical topology produced by the Python DSL and consumed by the arbiter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalTopology {
    pub boards: Vec<BoardSpec>,
    pub edges: Vec<DirectedEdge>,
    /// Required virtual-time skew margin (nanoseconds). No implicit default.
    pub margin_ns: u64,
    /// Required headroom threshold for time reports (nanoseconds). No default.
    pub headroom_threshold_ns: u64,
    /// Ready-barrier timeout; omitted → [`DEFAULT_READY_TIMEOUT_MS`].
    #[serde(default = "default_ready_timeout_ms")]
    pub ready_timeout_ms: u64,
}

fn default_ready_timeout_ms() -> u64 {
    DEFAULT_READY_TIMEOUT_MS
}

/// Topology parse / validation failures.
#[derive(Debug, thiserror::Error)]
pub enum TopologyError {
    #[error("invalid topology JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}

impl LogicalTopology {
    /// Parse JSON text and run structural validation.
    pub fn from_json_str(text: &str) -> Result<Self, TopologyError> {
        let topo: Self = serde_json::from_str(text)?;
        topo.validate()?;
        Ok(topo)
    }

    /// Serialize to JSON (stable field names for the Python DSL contract).
    pub fn to_json_string(&self) -> Result<String, TopologyError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Structural checks: unique ids, required fields, edge endpoint resolution.
    pub fn validate(&self) -> Result<(), TopologyError> {
        if self.boards.is_empty() {
            return Err(TopologyError::Invalid(
                "topology must declare at least one board".into(),
            ));
        }

        let mut board_ids = HashSet::new();
        for board in &self.boards {
            if board.id.is_empty() {
                return Err(TopologyError::Invalid("board id must not be empty".into()));
            }
            if !board_ids.insert(board.id.clone()) {
                return Err(TopologyError::Invalid(format!(
                    "duplicate board id '{}'",
                    board.id
                )));
            }
            let mut ep_names = HashSet::new();
            for ep in &board.endpoints {
                if ep.name.is_empty() {
                    return Err(TopologyError::Invalid(format!(
                        "board '{}': endpoint name must not be empty",
                        board.id
                    )));
                }
                if !ep_names.insert(ep.name.clone()) {
                    return Err(TopologyError::Invalid(format!(
                        "board '{}': duplicate endpoint '{}'",
                        board.id, ep.name
                    )));
                }
            }
        }

        for edge in &self.edges {
            let from = self
                .boards
                .iter()
                .find(|b| b.id == edge.from_board)
                .ok_or_else(|| {
                    TopologyError::Invalid(format!("edge from unknown board '{}'", edge.from_board))
                })?;
            let to = self
                .boards
                .iter()
                .find(|b| b.id == edge.to_board)
                .ok_or_else(|| {
                    TopologyError::Invalid(format!("edge to unknown board '{}'", edge.to_board))
                })?;
            let from_ep = from
                .endpoints
                .iter()
                .find(|e| e.name == edge.from_endpoint)
                .ok_or_else(|| {
                    TopologyError::Invalid(format!(
                        "edge: board '{}' has no endpoint '{}'",
                        edge.from_board, edge.from_endpoint
                    ))
                })?;
            let to_ep = to
                .endpoints
                .iter()
                .find(|e| e.name == edge.to_endpoint)
                .ok_or_else(|| {
                    TopologyError::Invalid(format!(
                        "edge: board '{}' has no endpoint '{}'",
                        edge.to_board, edge.to_endpoint
                    ))
                })?;
            if from_ep.direction != EndpointDirection::Out {
                return Err(TopologyError::Invalid(format!(
                    "edge source '{}:{}' must be direction out",
                    edge.from_board, edge.from_endpoint
                )));
            }
            if to_ep.direction != EndpointDirection::In {
                return Err(TopologyError::Invalid(format!(
                    "edge sink '{}:{}' must be direction in",
                    edge.to_board, edge.to_endpoint
                )));
            }
            if from_ep.payload != edge.payload || to_ep.payload != edge.payload {
                return Err(TopologyError::Invalid(format!(
                    "edge payload {:?} mismatches endpoint payloads",
                    edge.payload
                )));
            }
            if edge.from_board == edge.to_board {
                return Err(TopologyError::Invalid(
                    "edge must connect two different boards".into(),
                ));
            }
        }

        Ok(())
    }

    /// Effective UART ring length for an edge.
    #[must_use]
    pub fn uart_ring_len(edge: &DirectedEdge) -> u32 {
        edge.uart_ring_len.unwrap_or(DEFAULT_UART_RING_LEN)
    }
}

/// Parse and validate logical topology JSON.
pub fn parse_logical_topology(text: &str) -> Result<LogicalTopology, TopologyError> {
    LogicalTopology::from_json_str(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> &'static str {
        r#"{
          "boards": [
            {
              "id": "a",
              "kind": "rl78",
              "elf": "a.elf",
              "endpoints": [
                {"name": "uart0_tx", "direction": "out", "payload": "uart"},
                {"name": "uart0_rx", "direction": "in", "payload": "uart"}
              ]
            },
            {
              "id": "b",
              "kind": "rl78",
              "endpoints": [
                {"name": "uart0_tx", "direction": "out", "payload": "uart"},
                {"name": "uart0_rx", "direction": "in", "payload": "uart"}
              ]
            }
          ],
          "edges": [
            {
              "from_board": "a",
              "from_endpoint": "uart0_tx",
              "to_board": "b",
              "to_endpoint": "uart0_rx",
              "payload": "uart"
            },
            {
              "from_board": "b",
              "from_endpoint": "uart0_tx",
              "to_board": "a",
              "to_endpoint": "uart0_rx",
              "payload": "uart",
              "uart_ring_len": 128
            }
          ],
          "margin_ns": 1000,
          "headroom_threshold_ns": 200
        }"#
    }

    #[test]
    fn parses_two_board_uart_mesh() {
        let topo = parse_logical_topology(sample_json()).unwrap();
        assert_eq!(topo.boards.len(), 2);
        assert_eq!(topo.edges.len(), 2);
        assert_eq!(topo.margin_ns, 1000);
        assert_eq!(topo.headroom_threshold_ns, 200);
        assert_eq!(topo.ready_timeout_ms, DEFAULT_READY_TIMEOUT_MS);
        assert_eq!(LogicalTopology::uart_ring_len(&topo.edges[0]), 64);
        assert_eq!(LogicalTopology::uart_ring_len(&topo.edges[1]), 128);
    }

    #[test]
    fn rejects_missing_margin() {
        let bad = sample_json().replace("\"margin_ns\": 1000,", "");
        assert!(parse_logical_topology(&bad).is_err());
    }

    #[test]
    fn rejects_unknown_edge_board() {
        let bad = sample_json().replace("\"to_board\": \"b\"", "\"to_board\": \"z\"");
        let err = parse_logical_topology(&bad).unwrap_err();
        assert!(err.to_string().contains("unknown board"));
    }

    #[test]
    fn round_trip_json() {
        let topo = parse_logical_topology(sample_json()).unwrap();
        let text = topo.to_json_string().unwrap();
        let again = parse_logical_topology(&text).unwrap();
        assert_eq!(topo, again);
    }
}
