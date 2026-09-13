//! Cluster node: outer loop with iceoryx control + UART (Running only).

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crate::control::{ControlToArbiter, ControlToNode, HostStopReason};
use crate::ipc::{
    IpcError, NodeControl, NodeUartPorts, board_hash_table, board_id_hash, create_node,
    isolated_config,
};
use crate::lifecycle::{NodeEffect, NodeState};
use crate::topology::{LogicalTopology, TopologyError};

/// Idle sleep when not Running (Q21: timed sleep + try_receive).
pub const NODE_IDLE_POLL: Duration = Duration::from_millis(1);

/// Sleep while Running so control/UART polling does not spin the CPU or flood n2a.
const NODE_RUNNING_POLL: Duration = Duration::from_millis(5);

/// Node-side failures.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("topology error: {0}")]
    Topology(#[from] TopologyError),
    #[error("ipc error: {0}")]
    Ipc(#[from] IpcError),
    #[error("{0}")]
    Message(String),
}

/// Optional HostStop injection for smoke / E2E without a wired Machine.
///
/// TODO(inject-host-stop): Timed `--inject-after-ms` is brittle (handshake vs
/// Running races). Prefer a barrier-style smoke path: nodes wait stopped until
/// arbiter Start after all peers are Ready — no wall-clock inject delay.
/// Tracking: https://github.com/Soya-Onishi/simulation-rl78-core/issues/41
#[derive(Clone, Debug)]
pub struct InjectHostStop {
    pub after_ms: u64,
    pub reason: String,
}

/// Helper used by the binary for typed args.
#[derive(Clone, Debug)]
pub struct NodeOptions {
    pub board_id: String,
    pub cluster_key: String,
    pub iox_root: PathBuf,
    pub topology_path: PathBuf,
    /// When set, send HostStop after Start (MVP smoke without Machine).
    pub inject_host_stop: Option<InjectHostStop>,
}

/// Drive the node outer loop until Stopped.
pub fn run_node(opts: NodeOptions) -> Result<(), NodeError> {
    let board_id = opts.board_id.as_str();
    let board_hash = board_id_hash(board_id);
    let text = fs::read_to_string(&opts.topology_path)?;
    let topo = LogicalTopology::from_json_str(&text)?;
    let boards =
        board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).map_err(NodeError::Message)?;

    let config = isolated_config(&opts.iox_root)?;
    let iox_node = create_node(&config, &format!("node-{board_id}-{}", opts.cluster_key))?;
    let control = NodeControl::open(&iox_node, &opts.cluster_key, boards)?;
    let uart = NodeUartPorts::open_for_board(&iox_node, &opts.cluster_key, board_id, &topo.edges)?;

    let mut state = NodeState::AwaitingStartup;
    let mut headroom_threshold_ns = 0_u64;
    let mut allowed_ns = 0_u64;
    let mut virtual_time_ns = 0_u64;
    let mut host_stop_sent = false;
    // Instant when we first entered Running (base for --inject-after-ms).
    let mut running_started: Option<Instant> = None;
    // Overall smoke window once Running (matches prior MVP brief run).
    let mut running_deadline: Option<Instant> = None;
    let mut last_time_report: Option<u64> = None;

    while state != NodeState::Stopped {
        // Control receive every iteration.
        while let Some(msg) = control.try_recv()? {
            if let ControlToNode::StartupRecord {
                headroom_threshold_ns: thr,
                ..
            } = msg
                && msg.is_for(board_hash)
            {
                headroom_threshold_ns = thr;
            }
            let (next, effect) = state.on_message(board_hash, msg);
            if let Some(effect) = effect {
                apply_effect(board_id, board_hash, &control, effect, &mut allowed_ns)?;
            }
            if next != state {
                eprintln!("cluster-node[{board_id}]: {state:?} -> {next:?}");
            }
            state = next;
            if state == NodeState::Running && running_started.is_none() {
                let now = Instant::now();
                running_started = Some(now);
                running_deadline = Some(now + Duration::from_millis(200));
            }
        }

        if state == NodeState::Running {
            // UART drain only while Running (Q19=B).
            let drained = uart.drain_all(64)?;
            for (edge_id, frames) in drained {
                if !frames.is_empty() {
                    eprintln!(
                        "cluster-node[{board_id}]: uart recv {} frames on {edge_id}",
                        frames.len()
                    );
                }
            }

            if let Some(inj) = opts.inject_host_stop.as_ref()
                && !host_stop_sent
                && let Some(started) = running_started
                && started.elapsed() >= Duration::from_millis(inj.after_ms)
            {
                match notify_host_stop(&control, board_id, &inj.reason)? {
                    true => {
                        eprintln!(
                            "cluster-node[{board_id}]: HostStop injected ({})",
                            inj.reason
                        );
                        break;
                    }
                    false => {
                        eprintln!(
                            "cluster-node[{board_id}]: inject HostStop ignored (non-cluster reason {})",
                            inj.reason
                        );
                        host_stop_sent = true;
                    }
                }
            }

            // TODO(phase6): Remove placeholder clock once Machine is wired; virtual
            // time must come from sim-kernel's clock via run_quantum in this
            // Running branch. Always account for the quantum that actually ran
            // (do not only creep while virtual_time_ns < allowed_ns); TimeReport
            // / ceiling sync then use that real virtual time, with arbiter still
            // taking min(reports)+margin.
            if virtual_time_ns < allowed_ns {
                virtual_time_ns = (virtual_time_ns + 1).min(allowed_ns);
            }
            let headroom = allowed_ns.saturating_sub(virtual_time_ns);
            // MVP: placeholder +1 makes "last virtual_time" dedupe ineffective;
            // sleep above limits spin. Real run_quantum advances in larger steps
            // and should drive TimeReport from the sim clock / headroom policy.
            if headroom < headroom_threshold_ns && last_time_report != Some(virtual_time_ns) {
                let _ = control.publish(&ControlToArbiter::TimeReport {
                    from: board_hash,
                    virtual_time_ns,
                });
                last_time_report = Some(virtual_time_ns);
            }

            // Final form will call run_quantum here; MVP exits after short window.
            if running_deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            thread::sleep(NODE_RUNNING_POLL);
        } else {
            thread::sleep(NODE_IDLE_POLL);
        }
    }
    Ok(())
}

/// Notify the arbiter of a host-initiated stop when the reason is cluster-relevant.
///
/// Returns `Ok(true)` if [`ControlToArbiter::HostStop`] was sent.
pub fn notify_host_stop(
    control: &NodeControl,
    board_id: &str,
    reason: &str,
) -> Result<bool, NodeError> {
    let Some(reason) = HostStopReason::from_label(reason) else {
        return Ok(false);
    };
    control.publish(&ControlToArbiter::HostStop {
        from: board_id_hash(board_id),
        reason,
    })?;
    Ok(true)
}

fn apply_effect(
    board_id: &str,
    board_hash: u64,
    control: &NodeControl,
    effect: NodeEffect,
    allowed_ns: &mut u64,
) -> Result<(), NodeError> {
    match effect {
        NodeEffect::SendReady => {
            control.publish(&ControlToArbiter::Ready { from: board_hash })?;
            eprintln!("cluster-node[{board_id}]: Ready");
        }
        NodeEffect::SetAllowed { allowed_ns: next } => {
            *allowed_ns = next;
            eprintln!("cluster-node[{board_id}]: Allowed={next}");
        }
        NodeEffect::Warn(msg) => {
            eprintln!("cluster-node[{board_id}]: warning: {msg}");
        }
    }
    Ok(())
}
