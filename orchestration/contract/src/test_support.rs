//! Helpers for tests that run iceoryx services (`test-support` feature).

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::command2node::rr::NodeCommandClient;
use crate::id::{ClusterId, HashedId};
use crate::ipc::{IoxNode, create_iox_node, isolated_config};

/// A cluster id no other test uses.
///
/// Service shared memory is keyed by name only, so concurrent tests (and test
/// binaries) must not share service paths.
#[must_use]
pub fn unique_cluster() -> ClusterId {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    ClusterId::new(std::process::id() ^ (seq << 24), nanos)
}

/// Command-side iceoryx node and control client under `iox_root`.
///
/// # Panics
/// If the iceoryx node or client cannot be created.
#[must_use]
pub fn node_command_client(
    iox_root: &Path,
    cluster: ClusterId,
    node_key: HashedId<u64>,
) -> (IoxNode, NodeCommandClient) {
    let cfg = isolated_config(iox_root).expect("iceoryx config");
    let node = create_iox_node(&cfg, "test-command").expect("iceoryx node");
    let client = NodeCommandClient::create(&node, cluster, node_key).expect("node command client");
    (node, client)
}
