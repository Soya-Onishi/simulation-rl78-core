//! iceoryx service path helpers (same-host Command ↔ Node / dataplane / probe).
//!
//! Cluster identity is [`ClusterId`] (`Display` = `"{id}-{nano}"` path segment).
//! Node keys are [`HashedId<u64>`] (16 lowercase hex digits).

use crate::cluster_id::ClusterId;
use crate::hashed_id::HashedId;

/// Probe publisher path suffix under the cluster key.
pub const PROBE_SERVICE_SUFFIX: &str = "probe";

/// Build `sim-cluster/{cluster}/probe` as specified in #141.
#[must_use]
pub fn probe_service_path(cluster: ClusterId) -> String {
    format!("sim-cluster/{cluster}/{PROBE_SERVICE_SUFFIX}")
}

/// Node inbound endpoint fan-in service: one subscriber per node.
///
/// Publishers: every other node on the host plus the Command Endpoint thread
/// (`max_publishers = host_node_count + 1`).
#[must_use]
pub fn node_endpoint_rx_path(cluster: ClusterId, node: HashedId<u64>) -> String {
    format!("sim-cluster/{cluster}/node/{node}/endpoint/rx")
}

/// Command ↔ Node request-response service stem.
#[must_use]
pub fn node_command_rr_path(cluster: ClusterId, node: HashedId<u64>) -> String {
    format!("sim-cluster/{cluster}/node/{node}/cmd")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_path_uses_cluster_display() {
        let c = ClusterId::new(0x11, 0x22);
        assert_eq!(
            probe_service_path(c),
            "sim-cluster/17-34/probe"
        );
    }

    #[test]
    fn node_paths_include_hashed_node() {
        let c = ClusterId::new(1, 2);
        let n = HashedId::new(0xdead_beef);
        assert_eq!(
            node_endpoint_rx_path(c, n),
            "sim-cluster/1-2/node/00000000deadbeef/endpoint/rx"
        );
        assert_eq!(
            node_command_rr_path(c, n),
            "sim-cluster/1-2/node/00000000deadbeef/cmd"
        );
    }
}
