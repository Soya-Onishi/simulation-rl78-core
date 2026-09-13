//! Deterministic iceoryx2 service names for a cluster instance.

use xxhash_rust::xxh3::xxh3_64;

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
///
/// Uses a content hash of the full logical [`edge_id`] so distinct edges cannot
/// collide after character sanitization (e.g. `:` / `->` both mapped away).
#[must_use]
pub fn uart_edge(cluster_key: &str, edge_id: &str) -> String {
    let digest = xxh3_64(edge_id.as_bytes());
    format!("sim-cluster/{cluster_key}/uart/{digest:016x}")
}

/// Default edge id string (matches previous arbiter formatting).
#[must_use]
pub fn edge_id(from_board: &str, from_ep: &str, to_board: &str, to_ep: &str) -> String {
    format!("{from_board}:{from_ep}->{to_board}:{to_ep}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uart_edge_names_distinguish_similar_edges() {
        let a = edge_id("a", "tx", "b", "rx");
        let b = edge_id("a_tx", "", "b_rx", "");
        // Sanitizing both to underscores would collide; hashes must differ.
        assert_ne!(uart_edge("k", &a), uart_edge("k", &b));
        assert_eq!(uart_edge("k", &a), uart_edge("k", &a));
    }
}
