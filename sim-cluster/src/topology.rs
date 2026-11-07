//! Logical topology JSON schema (runtime handoff from the Python DSL).
//!
//! This is not a checked-in product config. The client emits JSON; the
//! singleton [`crate::server`] validates it and starts the arbiter.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Default Ready-barrier timeout when omitted from topology JSON (milliseconds).
pub const DEFAULT_READY_TIMEOUT_MS: u64 = 10_000;

/// Default UART ring capacity (frames) when an edge omits `uart_ring_len`.
///
/// Sized for ~1 ms of backlog at one frame per 4 µs of virtual time (250),
/// rounded up to 256.
pub const DEFAULT_UART_RING_LEN: u32 = 256;

/// Payload carried on a board-edge endpoint / directed data edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadKind {
    Uart,
    Digital,
    Analog,
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
    /// Optional guest firmware source for the node to load.
    ///
    /// One string selects exactly one source: a filesystem path, `file://`,
    /// `http://` or `https://`, or `base64:` plus a standard Base64
    /// payload. A path and inline bytes are not combined, and a JSON object
    /// is rejected. See [`crate::elf_source`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elf: Option<String>,
    pub endpoints: Vec<EndpointSpec>,
}

/// One hop on a directed edge: leaf board/plant or intermediate topology export.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRef {
    pub path: String,
    pub port: String,
}

/// Directed data-plane edge between two leaf endpoints.
///
/// `from` / `to` are always leaf boards. `route` lists display hops
/// (`route[0] == from`, `route[last] == to`); the data plane wires only the
/// leaf endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectedEdge {
    pub from: PortRef,
    pub to: PortRef,
    pub route: Vec<PortRef>,
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
            if let Some(elf) = &board.elf {
                crate::elf_source::check_spec(elf).map_err(|err| {
                    TopologyError::Invalid(format!("board '{}': {err}", board.id))
                })?;
            }
        }

        let mut edge_keys = HashSet::new();
        for edge in &self.edges {
            if edge.route.is_empty() {
                return Err(TopologyError::Invalid(
                    "edge route must not be empty".into(),
                ));
            }
            if edge.route.first() != Some(&edge.from) {
                return Err(TopologyError::Invalid(
                    "edge route[0] must equal from".into(),
                ));
            }
            if edge.route.last() != Some(&edge.to) {
                return Err(TopologyError::Invalid(
                    "edge route[last] must equal to".into(),
                ));
            }
            let key = (
                edge.from.path.clone(),
                edge.from.port.clone(),
                edge.to.path.clone(),
                edge.to.port.clone(),
            );
            if !edge_keys.insert(key) {
                return Err(TopologyError::Invalid(format!(
                    "duplicate edge '{}:{}' -> '{}:{}'",
                    edge.from.path, edge.from.port, edge.to.path, edge.to.port
                )));
            }
            let from = self
                .boards
                .iter()
                .find(|b| b.id == edge.from.path)
                .ok_or_else(|| {
                    TopologyError::Invalid(format!("edge from unknown board '{}'", edge.from.path))
                })?;
            let to = self
                .boards
                .iter()
                .find(|b| b.id == edge.to.path)
                .ok_or_else(|| {
                    TopologyError::Invalid(format!("edge to unknown board '{}'", edge.to.path))
                })?;
            let from_ep = from
                .endpoints
                .iter()
                .find(|e| e.name == edge.from.port)
                .ok_or_else(|| {
                    TopologyError::Invalid(format!(
                        "edge: board '{}' has no endpoint '{}'",
                        edge.from.path, edge.from.port
                    ))
                })?;
            let to_ep = to
                .endpoints
                .iter()
                .find(|e| e.name == edge.to.port)
                .ok_or_else(|| {
                    TopologyError::Invalid(format!(
                        "edge: board '{}' has no endpoint '{}'",
                        edge.to.path, edge.to.port
                    ))
                })?;
            if from_ep.direction != EndpointDirection::Out {
                return Err(TopologyError::Invalid(format!(
                    "edge source '{}:{}' must be direction out",
                    edge.from.path, edge.from.port
                )));
            }
            if to_ep.direction != EndpointDirection::In {
                return Err(TopologyError::Invalid(format!(
                    "edge sink '{}:{}' must be direction in",
                    edge.to.path, edge.to.port
                )));
            }
            if from_ep.payload != edge.payload || to_ep.payload != edge.payload {
                return Err(TopologyError::Invalid(format!(
                    "edge payload {:?} mismatches endpoint payloads",
                    edge.payload
                )));
            }
            if edge.from.path == edge.to.path {
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

/// Reverse lookups for path / port / edge hashes computed at topology load.
#[derive(Clone, Debug)]
pub struct TopologyHashes {
    pub boards: std::collections::HashMap<u64, String>,
    pub paths: std::collections::HashMap<u64, String>,
    pub ports: std::collections::HashMap<u64, String>,
    pub edges: std::collections::HashMap<u64, String>,
}

/// Hash every path, port name, and leaf edge id; fail on distinct-string collisions.
pub fn build_topology_hashes(topo: &LogicalTopology) -> Result<TopologyHashes, TopologyError> {
    use crate::ipc::{
        board_hash_table, edge_hash_table, edge_id_logical, path_hash_table, port_hash_table,
    };

    let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str()))
        .map_err(TopologyError::Invalid)?;

    let mut paths: Vec<&str> = topo.boards.iter().map(|b| b.id.as_str()).collect();
    for edge in &topo.edges {
        paths.push(edge.from.path.as_str());
        paths.push(edge.to.path.as_str());
        for hop in &edge.route {
            paths.push(hop.path.as_str());
        }
    }
    let paths = path_hash_table(paths).map_err(TopologyError::Invalid)?;

    let mut ports: Vec<&str> = Vec::new();
    for board in &topo.boards {
        for ep in &board.endpoints {
            ports.push(ep.name.as_str());
        }
    }
    for edge in &topo.edges {
        ports.push(edge.from.port.as_str());
        ports.push(edge.to.port.as_str());
        for hop in &edge.route {
            ports.push(hop.port.as_str());
        }
    }
    let ports = port_hash_table(ports).map_err(TopologyError::Invalid)?;

    let logical: Vec<String> = topo
        .edges
        .iter()
        .map(|edge| {
            edge_id_logical(
                &edge.from.path,
                &edge.from.port,
                &edge.to.path,
                &edge.to.port,
            )
        })
        .collect();
    let edges = edge_hash_table(logical.iter().map(String::as_str)).map_err(TopologyError::Invalid)?;

    Ok(TopologyHashes {
        boards,
        paths,
        ports,
        edges,
    })
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
              "from": {"path": "a", "port": "uart0_tx"},
              "to": {"path": "b", "port": "uart0_rx"},
              "route": [
                {"path": "a", "port": "uart0_tx"},
                {"path": "b", "port": "uart0_rx"}
              ],
              "payload": "uart"
            },
            {
              "from": {"path": "b", "port": "uart0_tx"},
              "to": {"path": "a", "port": "uart0_rx"},
              "route": [
                {"path": "b", "port": "uart0_tx"},
                {"path": "a", "port": "uart0_rx"}
              ],
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
        assert_eq!(LogicalTopology::uart_ring_len(&topo.edges[0]), 256);
        assert_eq!(LogicalTopology::uart_ring_len(&topo.edges[1]), 128);
        assert_eq!(topo.edges[0].from.path, "a");
        assert_eq!(topo.edges[0].route.len(), 2);
        let hashes = build_topology_hashes(&topo).unwrap();
        assert_eq!(hashes.boards.len(), 2);
        assert_eq!(hashes.edges.len(), 2);
        assert!(hashes.ports.contains_key(&crate::ipc::port_id_hash("uart0_tx")));
    }

    #[test]
    fn rejects_missing_margin() {
        let bad = sample_json().replace("\"margin_ns\": 1000,", "");
        assert!(parse_logical_topology(&bad).is_err());
    }

    #[test]
    fn rejects_unknown_edge_board() {
        let bad = r#"{
          "boards": [{"id":"a","kind":"rl78","endpoints":[
            {"name":"tx","direction":"out","payload":"uart"}
          ]}],
          "edges": [{
            "from": {"path":"a","port":"tx"},
            "to": {"path":"z","port":"rx"},
            "route": [
              {"path":"a","port":"tx"},
              {"path":"z","port":"rx"}
            ],
            "payload":"uart"
          }],
          "margin_ns": 1,
          "headroom_threshold_ns": 1
        }"#;
        let err = parse_logical_topology(bad).unwrap_err();
        assert!(err.to_string().contains("unknown board"));
    }

    #[test]
    fn round_trip_json() {
        let topo = parse_logical_topology(sample_json()).unwrap();
        let text = topo.to_json_string().unwrap();
        let again = parse_logical_topology(&text).unwrap();
        assert_eq!(topo, again);
    }

    #[test]
    fn rejects_duplicate_leaf_edge() {
        let text = r#"{
          "boards": [
            {"id":"a","kind":"rl78","endpoints":[
              {"name":"tx","direction":"out","payload":"uart"}
            ]},
            {"id":"b","kind":"rl78","endpoints":[
              {"name":"rx","direction":"in","payload":"uart"}
            ]}
          ],
          "edges": [
            {
              "from": {"path":"a","port":"tx"},
              "to": {"path":"b","port":"rx"},
              "route": [
                {"path":"a","port":"tx"},
                {"path":"b","port":"rx"}
              ],
              "payload":"uart"
            },
            {
              "from": {"path":"a","port":"tx"},
              "to": {"path":"b","port":"rx"},
              "route": [
                {"path":"a","port":"tx"},
                {"path":"b","port":"rx"}
              ],
              "payload":"uart"
            }
          ],
          "margin_ns": 1,
          "headroom_threshold_ns": 1
        }"#;
        let err = parse_logical_topology(text).unwrap_err();
        assert!(err.to_string().contains("duplicate edge"));
    }

    #[test]
    fn rejects_empty_route() {
        let text = r#"{
          "boards": [
            {"id":"a","kind":"rl78","endpoints":[
              {"name":"tx","direction":"out","payload":"uart"}
            ]},
            {"id":"b","kind":"rl78","endpoints":[
              {"name":"rx","direction":"in","payload":"uart"}
            ]}
          ],
          "edges": [{
            "from": {"path":"a","port":"tx"},
            "to": {"path":"b","port":"rx"},
            "route": [],
            "payload":"uart"
          }],
          "margin_ns": 1,
          "headroom_threshold_ns": 1
        }"#;
        let err = parse_logical_topology(text).unwrap_err();
        assert!(err.to_string().contains("route must not be empty"));
    }

    #[test]
    fn rejects_route_mismatch_ends() {
        let bad_first = r#"{
          "boards": [
            {"id":"a","kind":"rl78","endpoints":[
              {"name":"tx","direction":"out","payload":"uart"}
            ]},
            {"id":"b","kind":"rl78","endpoints":[
              {"name":"rx","direction":"in","payload":"uart"}
            ]}
          ],
          "edges": [{
            "from": {"path":"a","port":"tx"},
            "to": {"path":"b","port":"rx"},
            "route": [
              {"path":"b","port":"rx"},
              {"path":"b","port":"rx"}
            ],
            "payload":"uart"
          }],
          "margin_ns": 1,
          "headroom_threshold_ns": 1
        }"#;
        let err = parse_logical_topology(bad_first).unwrap_err();
        assert!(err.to_string().contains("route[0] must equal from"));

        let bad_last = r#"{
          "boards": [
            {"id":"a","kind":"rl78","endpoints":[
              {"name":"tx","direction":"out","payload":"uart"}
            ]},
            {"id":"b","kind":"rl78","endpoints":[
              {"name":"rx","direction":"in","payload":"uart"}
            ]}
          ],
          "edges": [{
            "from": {"path":"a","port":"tx"},
            "to": {"path":"b","port":"rx"},
            "route": [
              {"path":"a","port":"tx"},
              {"path":"a","port":"tx"}
            ],
            "payload":"uart"
          }],
          "margin_ns": 1,
          "headroom_threshold_ns": 1
        }"#;
        let err = parse_logical_topology(bad_last).unwrap_err();
        assert!(err.to_string().contains("route[last] must equal to"));
    }

    #[test]
    fn accepts_hierarchical_route_hops() {
        let text = r#"{
          "boards": [
            {
              "id": "sys/left/a",
              "kind": "rl78",
              "endpoints": [
                {"name": "tx", "direction": "out", "payload": "uart"}
              ]
            },
            {
              "id": "sys/d",
              "kind": "rl78",
              "endpoints": [
                {"name": "rx", "direction": "in", "payload": "uart"}
              ]
            }
          ],
          "edges": [
            {
              "from": {"path": "sys/left/a", "port": "tx"},
              "to": {"path": "sys/d", "port": "rx"},
              "route": [
                {"path": "sys/left/a", "port": "tx"},
                {"path": "sys/left", "port": "tx"},
                {"path": "sys/d", "port": "rx"}
              ],
              "payload": "uart"
            }
          ],
          "margin_ns": 1,
          "headroom_threshold_ns": 1
        }"#;
        let topo = parse_logical_topology(text).unwrap();
        assert_eq!(topo.edges[0].route.len(), 3);
        assert_eq!(topo.edges[0].route[1].path, "sys/left");
        let again = parse_logical_topology(&topo.to_json_string().unwrap()).unwrap();
        assert_eq!(topo.edges[0].route, again.edges[0].route);
    }

    #[test]
    fn accepts_remote_and_inline_elf_strings() {
        for elf in ["a.elf", "https://example.test/fw.elf", "base64:AQI="] {
            let elf_json = serde_json::to_string(elf).unwrap();
            let text = format!(
                r#"{{"boards":[{{"id":"a","kind":"rl78","elf":{elf_json},"endpoints":[]}}],"edges":[],"margin_ns":1,"headroom_threshold_ns":1}}"#
            );
            let topo = parse_logical_topology(&text).unwrap();
            assert_eq!(topo.boards[0].elf.as_deref(), Some(elf));
        }
    }

    #[test]
    fn rejects_unsupported_elf_and_combined_object() {
        let scheme = r#"{"boards":[{"id":"a","kind":"rl78","elf":"ftp://example/a.elf","endpoints":[]}],"edges":[],"margin_ns":1,"headroom_threshold_ns":1}"#;
        let err = parse_logical_topology(scheme).unwrap_err();
        assert!(err.to_string().contains("unsupported elf URL scheme"));

        let object = r#"{"boards":[{"id":"a","kind":"rl78","elf":{"path":"a.elf","base64":"AQI="},"endpoints":[]}],"edges":[],"margin_ns":1,"headroom_threshold_ns":1}"#;
        assert!(parse_logical_topology(object).is_err());
    }
}
