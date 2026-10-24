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

/// Service name: nodes → arbiter log channel.
#[must_use]
pub fn log_n2a(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/log/n2a")
}

/// Service name: arbiter → server log channel.
#[must_use]
pub fn log_a2s(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/log/a2s")
}

/// Service name: server → arbiter host commands.
#[must_use]
pub fn ctrl_s2a(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/ctrl/s2a")
}

/// Service name: arbiter → server session status.
#[must_use]
pub fn status_a2s(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/status/a2s")
}

/// Service name: nodes → arbiter action catalog and results.
#[must_use]
pub fn action_n2a(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/action/n2a")
}

/// Service name: arbiter → nodes action requests.
#[must_use]
pub fn action_a2n(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/action/a2n")
}

/// Service name: server → arbiter action requests.
#[must_use]
pub fn action_s2a(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/action/s2a")
}

/// Service name: arbiter → server action catalog and results.
#[must_use]
pub fn action_a2s(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/action/a2s")
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

/// Service name for one board probe point.
#[must_use]
pub fn probe(cluster_key: &str, board_hash: u64, name_hash: u64) -> String {
    format!("sim-cluster/{cluster_key}/probe/{board_hash:016x}/{name_hash:016x}")
}

/// Service name: arbiter → server live probe samples.
#[must_use]
pub fn probe_a2s(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/probe/a2s")
}

/// Service name: arbiter → server probe catalog dump.
#[must_use]
pub fn probe_catalog_a2s(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/probe/catalog/a2s")
}

/// Service name: server → arbiter watch-set replace.
#[must_use]
pub fn probe_watch_s2a(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/probe/watch/s2a")
}

/// Service name: server → arbiter probe query.
#[must_use]
pub fn probe_query_s2a(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/probe/query/s2a")
}

/// Service name: arbiter → nodes probe query.
#[must_use]
pub fn probe_query_a2n(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/probe/query/a2n")
}

/// Service name: nodes → arbiter probe query reply.
#[must_use]
pub fn probe_query_n2a(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/probe/query/n2a")
}

/// Service name: arbiter → server probe query reply.
#[must_use]
pub fn probe_query_a2s(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/probe/query/a2s")
}

/// Service name: server → arbiter CSV export request.
#[must_use]
pub fn probe_csv_s2a(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/probe/csv/s2a")
}

/// Service name: arbiter → nodes CSV export request.
#[must_use]
pub fn probe_csv_a2n(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/probe/csv/a2n")
}

/// Service name: nodes → arbiter CSV export chunk.
#[must_use]
pub fn probe_csv_n2a(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/probe/csv/n2a")
}

/// Service name: arbiter → server CSV export chunk.
#[must_use]
pub fn probe_csv_a2s(cluster_key: &str) -> String {
    format!("sim-cluster/{cluster_key}/probe/csv/a2s")
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

    #[test]
    fn probe_service_names_are_stable() {
        let a = probe("k", 1, 2);
        assert_eq!(a, probe("k", 1, 2));
        assert_ne!(probe("k", 1, 2), probe("k", 1, 3));
        assert!(a.contains("/probe/"));
    }
}
