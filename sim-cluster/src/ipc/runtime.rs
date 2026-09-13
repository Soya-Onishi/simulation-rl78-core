//! iceoryx2 node + isolated config for a cluster instance.

use std::path::PathBuf;

use iceoryx2::config::Config;
use iceoryx2::prelude::*;
use iceoryx2_bb_system_types::path::Path as IoxPath;

/// Errors from building an isolated iceoryx runtime.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("{0}")]
    Message(String),
}

/// Absolute directory used as iceoryx `root_path` for this cluster.
#[must_use]
pub fn root_path_for_cluster(cluster_key: &str) -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("sim-cluster")
        .join(format!("iox-{cluster_key}"))
}

/// Build a Config with an isolated root path under `root`.
pub fn isolated_config(root: &std::path::Path) -> Result<Config, IpcError> {
    std::fs::create_dir_all(root).map_err(|e| IpcError::Message(e.to_string()))?;
    let mut cfg = Config::default();
    let bytes = path_as_bytes(root)?;
    let iox_path = IoxPath::new(&bytes).map_err(|e| {
        IpcError::Message(format!(
            "invalid iceoryx root path {}: {e:?}",
            root.display()
        ))
    })?;
    cfg.global.set_root_path(&iox_path);
    Ok(cfg)
}

fn path_as_bytes(path: &std::path::Path) -> Result<Vec<u8>, IpcError> {
    let s = path
        .to_str()
        .ok_or_else(|| IpcError::Message(format!("non-UTF8 path {}", path.display())))?;
    // iceoryx Path expects a trailing separator for directories in some versions;
    // ensure we pass an absolute-looking path without NUL.
    let mut owned = s.to_string();
    if !owned.ends_with('/') {
        owned.push('/');
    }
    Ok(owned.into_bytes())
}

/// Create an iceoryx node with signal handling disabled (cluster owns lifecycle).
pub fn create_node(config: &Config, name: &str) -> Result<Node<ipc::Service>, IpcError> {
    let node_name = NodeName::new(name).map_err(|e| IpcError::Message(format!("{e:?}")))?;
    NodeBuilder::new()
        .name(&node_name)
        .config(config)
        .signal_handling_mode(SignalHandlingMode::Disabled)
        .create::<ipc::Service>()
        .map_err(|e| IpcError::Message(format!("iceoryx Node create failed: {e:?}")))
}

/// Generate a unique cluster key for one arbiter run.
#[must_use]
pub fn new_cluster_key() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}
