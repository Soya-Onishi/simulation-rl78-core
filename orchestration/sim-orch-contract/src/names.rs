//! iceoryx service path helpers (same-host Command ↔ Node / dataplane / probe).
//!
//! Callers must pass already-validated hex cluster / node keys (no `/` or NUL).
//! Runtime owns hashing ([`HashedId`](sim_cluster style)); this module only
//! formats paths.

/// Probe publisher path suffix under the cluster key.
pub const PROBE_SERVICE_SUFFIX: &str = "probe";

/// `HashedId<u64>` public wire form: exactly 16 lowercase hex digits.
fn key_ok(key: &str) -> bool {
    key.len() == 16 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Build `sim-cluster/{cluster_key}/probe` as specified in #141.
///
/// Returns `None` if `cluster_key` is not non-empty ASCII hex.
#[must_use]
pub fn probe_service_path(cluster_key: &str) -> Option<String> {
    if !key_ok(cluster_key) {
        return None;
    }
    Some(format!("sim-cluster/{cluster_key}/{PROBE_SERVICE_SUFFIX}"))
}

/// Node inbound endpoint fan-in service: one subscriber per node.
///
/// Publishers: every other node on the host plus the Command Endpoint thread
/// (`max_publishers = host_node_count + 1`).
#[must_use]
pub fn node_endpoint_rx_path(cluster_key: &str, node_key: &str) -> Option<String> {
    if !key_ok(cluster_key) || !key_ok(node_key) {
        return None;
    }
    Some(format!(
        "sim-cluster/{cluster_key}/node/{node_key}/endpoint/rx"
    ))
}

/// Command ↔ Node request-response service stem.
#[must_use]
pub fn node_command_rr_path(cluster_key: &str, node_key: &str) -> Option<String> {
    if !key_ok(cluster_key) || !key_ok(node_key) {
        return None;
    }
    Some(format!("sim-cluster/{cluster_key}/node/{node_key}/cmd"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_path() {
        assert_eq!(
            probe_service_path("00000000deadbeef").as_deref(),
            Some("sim-cluster/00000000deadbeef/probe")
        );
        assert!(probe_service_path("deadbeef").is_none());
        assert!(probe_service_path("00000000DEADBEEF").is_none());
        assert!(probe_service_path("bad/key").is_none());
        assert!(probe_service_path("").is_none());
    }
}
