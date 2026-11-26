//! Logical topology JSON (same schema as `sim-cluster`'s `LogicalTopology`).
//!
//! The Orchestrator sends it in the `POST /sessions` body. Boards are keyed on
//! the wire by [`node_key`] of their id. The `elf` string form is checked where
//! the firmware is loaded, not here.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::id::HashedId;

/// Booting-barrier timeout when omitted from topology JSON (milliseconds).
pub const DEFAULT_BOOTING_TIMEOUT_MS: u64 = 10_000;

/// Payload carried on a board-edge endpoint / directed data edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadKind {
    Uart,
    Digital,
    Analog,
    Pulse,
    Scalar,
}

/// Direction of an endpoint relative to its board.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointDirection {
    In,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointSpec {
    pub name: String,
    pub direction: EndpointDirection,
    pub payload: PayloadKind,
}

/// One simulation board (one Node process).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSpec {
    pub id: String,
    /// Board kind (e.g. `"rl78"`, `"plant"`); selects the Node binary.
    pub kind: String,
    /// Firmware source: a path, `file://`, `http(s)://`, or `base64:` payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elf: Option<String>,
    pub endpoints: Vec<EndpointSpec>,
}

/// One hop on a directed edge: a leaf board or an intermediate topology export.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortRef {
    pub path: String,
    pub port: String,
}

/// Directed data edge between two leaf board endpoints.
///
/// `route` lists display hops (`route[0] == from`, `route[last] == to`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectedEdge {
    pub from: PortRef,
    pub to: PortRef,
    pub route: Vec<PortRef>,
    pub payload: PayloadKind,
    /// UART ring length; ignored for non-UART payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uart_ring_len: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalTopology {
    pub boards: Vec<BoardSpec>,
    pub edges: Vec<DirectedEdge>,
    /// Required virtual-time skew margin (nanoseconds).
    pub margin_ns: u64,
    #[serde(default = "default_booting_timeout_ms", alias = "ready_timeout_ms")]
    pub booting_timeout_ms: u64,
}

fn default_booting_timeout_ms() -> u64 {
    DEFAULT_BOOTING_TIMEOUT_MS
}

/// Structurally invalid topology.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopologyError(pub String);

impl fmt::Display for TopologyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TopologyError {}

fn invalid(message: impl Into<String>) -> TopologyError {
    TopologyError(message.into())
}

/// Wire key of the board with id `board_id` (`sim-cluster` `board_id_hash`).
#[must_use]
pub fn node_key(board_id: &str) -> HashedId<u64> {
    HashedId::new(sim_kernel::id_hash(board_id))
}

impl LogicalTopology {
    /// Board count; `host_ctrl` holds one virtual-time slot per board.
    #[must_use]
    pub fn board_count(&self) -> usize {
        self.boards.len()
    }

    /// Key → board for every board. Fails on duplicate ids or key collisions.
    pub fn boards_by_key(&self) -> Result<HashMap<HashedId<u64>, &BoardSpec>, TopologyError> {
        let mut boards = HashMap::with_capacity(self.boards.len());
        for board in &self.boards {
            if let Some(other) = boards.insert(node_key(&board.id), board) {
                return Err(if other.id == board.id {
                    invalid(format!("duplicate board id '{}'", board.id))
                } else {
                    invalid(format!(
                        "board ids '{}' and '{}' share a node key",
                        other.id, board.id
                    ))
                });
            }
        }
        Ok(boards)
    }

    /// Unique ids and endpoint names, and edges that join an `out` endpoint
    /// to an `in` endpoint of the same payload on another board.
    pub fn validate(&self) -> Result<(), TopologyError> {
        if self.boards.is_empty() {
            return Err(invalid("topology must declare at least one board"));
        }
        for board in &self.boards {
            if board.id.is_empty() {
                return Err(invalid("board id must not be empty"));
            }
            let mut names = HashSet::new();
            for endpoint in &board.endpoints {
                if endpoint.name.is_empty() {
                    return Err(invalid(format!(
                        "board '{}': endpoint name must not be empty",
                        board.id
                    )));
                }
                if !names.insert(endpoint.name.as_str()) {
                    return Err(invalid(format!(
                        "board '{}': duplicate endpoint '{}'",
                        board.id, endpoint.name
                    )));
                }
            }
        }
        self.boards_by_key()?;

        let mut edges = HashSet::new();
        for edge in &self.edges {
            self.validate_edge(edge)?;
            if !edges.insert((&edge.from, &edge.to)) {
                return Err(invalid(format!(
                    "duplicate edge '{}:{}' -> '{}:{}'",
                    edge.from.path, edge.from.port, edge.to.path, edge.to.port
                )));
            }
        }
        Ok(())
    }

    fn validate_edge(&self, edge: &DirectedEdge) -> Result<(), TopologyError> {
        if edge.route.is_empty() {
            return Err(invalid("edge route must not be empty"));
        }
        if edge.route.first() != Some(&edge.from) {
            return Err(invalid("edge route[0] must equal from"));
        }
        if edge.route.last() != Some(&edge.to) {
            return Err(invalid("edge route[last] must equal to"));
        }
        if edge.from.path == edge.to.path {
            return Err(invalid("edge must connect two different boards"));
        }
        let from = self.endpoint(&edge.from, "from")?;
        let to = self.endpoint(&edge.to, "to")?;
        if from.direction != EndpointDirection::Out {
            return Err(invalid(format!(
                "edge source '{}:{}' must be direction out",
                edge.from.path, edge.from.port
            )));
        }
        if to.direction != EndpointDirection::In {
            return Err(invalid(format!(
                "edge sink '{}:{}' must be direction in",
                edge.to.path, edge.to.port
            )));
        }
        if from.payload != edge.payload || to.payload != edge.payload {
            return Err(invalid(format!(
                "edge payload {:?} mismatches endpoint payloads",
                edge.payload
            )));
        }
        Ok(())
    }

    fn endpoint(&self, port: &PortRef, side: &str) -> Result<&EndpointSpec, TopologyError> {
        let board = self
            .boards
            .iter()
            .find(|b| b.id == port.path)
            .ok_or_else(|| invalid(format!("edge {side} unknown board '{}'", port.path)))?;
        board
            .endpoints
            .iter()
            .find(|e| e.name == port.port)
            .ok_or_else(|| {
                invalid(format!(
                    "edge: board '{}' has no endpoint '{}'",
                    port.path, port.port
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<LogicalTopology, TopologyError> {
        let topology: LogicalTopology =
            serde_json::from_str(text).map_err(|e| invalid(e.to_string()))?;
        topology.validate()?;
        Ok(topology)
    }

    fn two_boards(edges: &str) -> String {
        format!(
            r#"{{
              "boards": [
                {{"id": "a", "kind": "rl78", "elf": "a.elf", "endpoints": [
                  {{"name": "tx", "direction": "out", "payload": "uart"}},
                  {{"name": "rx", "direction": "in", "payload": "uart"}}
                ]}},
                {{"id": "b", "kind": "rl78", "endpoints": [
                  {{"name": "tx", "direction": "out", "payload": "uart"}},
                  {{"name": "rx", "direction": "in", "payload": "uart"}}
                ]}}
              ],
              "edges": [{edges}],
              "margin_ns": 1000
            }}"#
        )
    }

    fn edge(from: (&str, &str), to: (&str, &str), route: &[(&str, &str)]) -> String {
        let port = |(path, port): (&str, &str)| format!(r#"{{"path":"{path}","port":"{port}"}}"#);
        let route: Vec<_> = route.iter().copied().map(port).collect();
        format!(
            r#"{{"from":{},"to":{},"route":[{}],"payload":"uart"}}"#,
            port(from),
            port(to),
            route.join(",")
        )
    }

    fn direct(from: (&str, &str), to: (&str, &str)) -> String {
        edge(from, to, &[from, to])
    }

    fn error(text: &str) -> String {
        parse(text).unwrap_err().0
    }

    #[test]
    fn parses_uart_mesh_with_defaults() {
        let text = two_boards(&format!(
            "{},{}",
            direct(("a", "tx"), ("b", "rx")),
            direct(("b", "tx"), ("a", "rx"))
        ));
        let topology = parse(&text).unwrap();
        assert_eq!(topology.board_count(), 2);
        assert_eq!(topology.edges.len(), 2);
        assert_eq!(topology.booting_timeout_ms, DEFAULT_BOOTING_TIMEOUT_MS);
        let again: LogicalTopology =
            serde_json::from_str(&serde_json::to_string(&topology).unwrap()).unwrap();
        assert_eq!(again, topology);
    }

    #[test]
    fn accepts_legacy_ready_timeout_ms_alias() {
        let text =
            two_boards("").replace("\"margin_ns\"", "\"ready_timeout_ms\": 1234, \"margin_ns\"");
        assert_eq!(parse(&text).unwrap().booting_timeout_ms, 1234);
    }

    #[test]
    fn node_key_matches_board_id_hash() {
        assert_eq!(node_key("a"), HashedId::new(sim_kernel::id_hash("a")));
        let topology = parse(&two_boards("")).unwrap();
        let boards = topology.boards_by_key().unwrap();
        assert_eq!(boards[&node_key("b")].id, "b");
    }

    #[test]
    fn rejects_missing_margin() {
        assert!(parse(&two_boards("").replace("\"margin_ns\": 1000", "")).is_err());
    }

    #[test]
    fn rejects_empty_and_duplicate_boards() {
        assert!(error(r#"{"boards":[],"edges":[],"margin_ns":1}"#).contains("at least one board"));
        let duplicate = two_boards("").replace("\"id\": \"b\"", "\"id\": \"a\"");
        assert!(error(&duplicate).contains("duplicate board id 'a'"));
    }

    #[test]
    fn rejects_duplicate_endpoint() {
        let text = two_boards("").replace("\"name\": \"rx\"", "\"name\": \"tx\"");
        assert!(error(&text).contains("duplicate endpoint 'tx'"));
    }

    #[test]
    fn rejects_bad_edges() {
        let cases = [
            (direct(("a", "tx"), ("z", "rx")), "unknown board 'z'"),
            (direct(("a", "tx"), ("b", "nope")), "has no endpoint 'nope'"),
            (direct(("a", "rx"), ("b", "rx")), "must be direction out"),
            (direct(("a", "tx"), ("b", "tx")), "must be direction in"),
            (direct(("a", "tx"), ("a", "rx")), "two different boards"),
            (
                edge(("a", "tx"), ("b", "rx"), &[]),
                "route must not be empty",
            ),
            (
                edge(("a", "tx"), ("b", "rx"), &[("b", "rx"), ("b", "rx")]),
                "route[0] must equal from",
            ),
            (
                edge(("a", "tx"), ("b", "rx"), &[("a", "tx"), ("a", "tx")]),
                "route[last] must equal to",
            ),
        ];
        for (edge, expected) in cases {
            let message = error(&two_boards(&edge));
            assert!(
                message.contains(expected),
                "{expected:?} not in {message:?}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_edge_and_payload_mismatch() {
        let edge = direct(("a", "tx"), ("b", "rx"));
        assert!(error(&two_boards(&format!("{edge},{edge}"))).contains("duplicate edge"));
        let analog = edge.replace("\"uart\"", "\"analog\"");
        assert!(error(&two_boards(&analog)).contains("mismatches endpoint payloads"));
    }

    #[test]
    fn accepts_hierarchical_route_hops() {
        let text = two_boards(&edge(
            ("a", "tx"),
            ("b", "rx"),
            &[("a", "tx"), ("sys/left", "tx"), ("b", "rx")],
        ));
        assert_eq!(parse(&text).unwrap().edges[0].route[1].path, "sys/left");
    }
}
