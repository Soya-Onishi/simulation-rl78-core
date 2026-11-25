//! IPC thread: Command ↔ Node RR server and request routing (#147).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread;
use std::time::Duration;

use contract::{
    IpcError, NodeCommand, NodeCommandError, NodeCommandResult, create_iox_node,
    create_node_command_server, isolated_config,
};

use crate::runtime::{NodeConfig, NodeRuntimeError, Routed};

/// Sleep when the RR server has no pending request.
const IDLE_SLEEP: Duration = Duration::from_millis(1);

/// Serve RR requests until `shutdown` is raised or [`NodeCommand::Shutdown`]
/// is answered. `ready` reports whether the RR server was created.
pub(crate) fn ipc_loop(
    config: &NodeConfig,
    sim_tx: &Sender<Routed>,
    probe_tx: &Sender<Routed>,
    shutdown: &AtomicBool,
    ready: &SyncSender<Result<(), IpcError>>,
) -> Result<(), NodeRuntimeError> {
    let setup = isolated_config(&config.iox_root).and_then(|cfg| {
        let node = create_iox_node(&cfg, &format!("sim-node-{}-ipc", config.node_key))?;
        let server = create_node_command_server(&node, config.cluster, config.node_key)?;
        Ok((node, server))
    });
    let (_node, server) = match setup {
        Ok(ports) => {
            let _ = ready.send(Ok(()));
            ports
        }
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(NodeRuntimeError::Ipc(e));
        }
    };

    while !shutdown.load(Ordering::Acquire) {
        let request = server
            .receive()
            .map_err(|e| NodeRuntimeError::Ipc(IpcError(format!("receive: {e:?}"))))?;
        let Some(request) = request else {
            thread::sleep(IDLE_SLEEP);
            continue;
        };
        let command = *request.payload();
        let result = match command {
            NodeCommand::Shutdown => NodeCommandResult::Success,
            _ => route(command, sim_tx, probe_tx),
        };
        if let Err(e) = request.send_copy(result) {
            log::warn!("node command response dropped: {e:?}");
        }
        if command == NodeCommand::Shutdown {
            log::info!("shutdown requested by Command");
            break;
        }
    }
    Ok(())
}

fn route(
    command: NodeCommand,
    sim_tx: &Sender<Routed>,
    probe_tx: &Sender<Routed>,
) -> NodeCommandResult {
    if !command.counts_valid() {
        return NodeCommandResult::Error(NodeCommandError::BadParam);
    }
    let target = match command {
        NodeCommand::ProbeSubscribe { .. } => probe_tx,
        _ => sim_tx,
    };
    let (reply, reply_rx) = mpsc::sync_channel(1);
    if target.send(Routed { command, reply }).is_err() {
        return NodeCommandResult::Error(NodeCommandError::ImplFault);
    }
    reply_rx
        .recv()
        .unwrap_or(NodeCommandResult::Error(NodeCommandError::ImplFault))
}
