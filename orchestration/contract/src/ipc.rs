//! iceoryx runtime setup shared by Command and Node (#147 / #153).
//!
//! Every process of one cluster on a host uses the same `iox_root` as the
//! iceoryx `root_path`. Shared-memory segments are still named from the
//! service name alone, so clusters stay apart only because every service path
//! embeds the [`crate::ClusterId`] (see [`crate::names`]).

use std::fmt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use iceoryx2::config::Config;
use iceoryx2::prelude::*;
use iceoryx2_bb_system_types::path::Path as IoxPath;

/// iceoryx node type used by every orchestration process.
pub type IoxNode = Node<ipc::Service>;

/// iceoryx setup / port / transport failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpcError(pub String);

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for IpcError {}

impl IpcError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// iceoryx [`Config`] whose `root_path` is `iox_root` (created if missing).
pub fn isolated_config(iox_root: &Path) -> Result<Config, IpcError> {
    std::fs::create_dir_all(iox_root)
        .map_err(|e| IpcError::new(format!("create {}: {e}", iox_root.display())))?;
    let s = iox_root
        .to_str()
        .ok_or_else(|| IpcError::new(format!("non-UTF8 path {}", iox_root.display())))?;
    let mut owned = s.to_string();
    if !owned.ends_with('/') {
        owned.push('/');
    }
    let iox_path = IoxPath::new(owned.as_bytes()).map_err(|e| {
        IpcError::new(format!(
            "invalid iceoryx root path {}: {e:?}",
            iox_root.display()
        ))
    })?;
    let mut cfg = Config::default();
    cfg.global.set_root_path(&iox_path);
    Ok(cfg)
}

/// Wait for another process already cleaning the same dead node.
const DEAD_NODE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const DEAD_NODE_RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Remove the resources of iceoryx nodes under `config` whose process died
/// without releasing them (e.g. a SIGKILLed Node).
///
/// Returns how many nodes remain: alive, or not identifiable as dead.
pub fn remove_dead_nodes(config: &Config) -> Result<usize, IpcError> {
    let deadline = Instant::now() + DEAD_NODE_CLEANUP_TIMEOUT;
    loop {
        match remove_dead_nodes_once(config) {
            // Another process (e.g. a starting Node) cleaning or opening the
            // same services concurrently can fail a pass transiently.
            Err(_) if Instant::now() < deadline => thread::sleep(DEAD_NODE_RETRY_INTERVAL),
            result => return result,
        }
    }
}

fn remove_dead_nodes_once(config: &Config) -> Result<usize, IpcError> {
    let mut dead = Vec::new();
    let mut remaining = 0;
    Node::<ipc::Service>::list(config, |state| {
        match state {
            NodeState::Dead(view) => dead.push(view),
            NodeState::Alive(_) | NodeState::Inaccessible(_) | NodeState::Undefined(_) => {
                remaining += 1;
            }
        }
        CallbackProgression::Continue
    })
    .map_err(|e| IpcError::new(format!("list iceoryx nodes: {e:?}")))?;
    for view in dead {
        view.blocking_remove_stale_resources(DEAD_NODE_CLEANUP_TIMEOUT)
            .map_err(|e| IpcError::new(format!("remove dead iceoryx node: {e:?}")))?;
    }
    Ok(remaining)
}

/// Create an iceoryx node with signal handling disabled: processes are
/// stopped by [`crate::NodeCommand::Shutdown`], not by signals.
pub fn create_iox_node(config: &Config, name: &str) -> Result<IoxNode, IpcError> {
    let node_name =
        NodeName::new(name).map_err(|e| IpcError::new(format!("node name {name}: {e:?}")))?;
    NodeBuilder::new()
        .name(&node_name)
        .config(config)
        .signal_handling_mode(SignalHandlingMode::Disabled)
        .create::<ipc::Service>()
        .map_err(|e| IpcError::new(format!("iceoryx node {name}: {e:?}")))
}
