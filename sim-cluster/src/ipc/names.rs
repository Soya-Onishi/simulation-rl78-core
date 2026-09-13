//! Deterministic iceoryx2 service names for a cluster instance.

/// Service name: arbiter → nodes control channel.
#[must_use]
pub fn ctrl_a2n(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/ctrl/a2n")
}

/// Service name: nodes → arbiter control channel.
#[must_use]
pub fn ctrl_n2a(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/ctrl/n2a")
}

/// Service name for one directed UART edge.
#[must_use]
pub fn uart_edge(cluster_key: &str, edge_id: &str) -> String {
    // ServiceName forbids some chars; keep edge_id printable ASCII and slash-safe.
    let safe: String = edge_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' => c,
            _ => '_',
        })
        .collect();
    format!("sim-cluster/{cluster_key}/uart/{safe}")
}

/// Default edge id string (matches previous arbiter formatting).
#[must_use]
pub fn edge_id(from_board: &str, from_ep: &str, to_board: &str, to_ep: &str) -> String {
    format!("{from_board}:{from_ep}->{to_board}:{to_ep}")
}
