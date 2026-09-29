//! Arbiter: create iceoryx services, spawn nodes, Ready barrier, then wait.
//!
//! The guest stays stopped until a host `start` command. `stop` halts the
//! guest without exiting this process, so a later `start` or `reset` can
//! run. `shutdown` on the server→arbiter iceoryx channel ends the process.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::control::{ControlToArbiter, ControlToNode, HostStopReason};
use crate::host::{HostCommand, SessionNode, SessionStatus};
use crate::ipc::{
    ArbiterControl, ArbiterHostPort, ArbiterLogBus, IpcError, board_hash_table, create_node,
    isolated_config, new_cluster_key, root_path_for_cluster,
};
use crate::lifecycle::{PeerEffect, PeerState};
use crate::time_sync::TimeCeiling;
use crate::topology::{LogicalTopology, TopologyError};

/// Idle poll while waiting for Ready / time-sync (arbiter side).
const ARBITER_POLL_IDLE: Duration = Duration::from_millis(1);

/// Arbiter failures.
#[derive(Debug, thiserror::Error)]
pub enum ArbiterError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("topology error: {0}")]
    Topology(#[from] TopologyError),
    #[error("ready barrier timed out after {timeout_ms}ms; missing boards: {missing}")]
    ReadyTimeout { timeout_ms: u64, missing: String },
    #[error("ipc error: {0}")]
    Ipc(#[from] IpcError),
    #[error("{0}")]
    Message(String),
}

/// Options for [`run_arbiter`].
#[derive(Clone, Debug)]
pub struct ArbiterOptions {
    pub topology_path: PathBuf,
    pub node_bin: PathBuf,
    /// Unique key for this cluster instance (iceoryx isolation + service names).
    pub cluster_key: String,
    /// Absolute iceoryx root path for this cluster.
    pub iox_root: PathBuf,
}

impl ArbiterOptions {
    pub fn from_topology_path(topology_path: PathBuf) -> Result<Self, ArbiterError> {
        let exe = std::env::current_exe().map_err(ArbiterError::Io)?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| ArbiterError::Message("current_exe has no parent".into()))?;
        let cluster_key = new_cluster_key();
        let iox_root = root_path_for_cluster(&cluster_key);
        Ok(Self {
            topology_path,
            node_bin: exe_dir.join("rl78-minimal-board"),
            cluster_key,
            iox_root,
        })
    }
}

/// Load topology, spawn nodes, wait for Ready, then follow iceoryx host commands.
pub fn run_arbiter(opts: &ArbiterOptions) -> Result<(), ArbiterError> {
    let text = fs::read_to_string(&opts.topology_path)?;
    let topo = LogicalTopology::from_json_str(&text)?;
    run_arbiter_with_topology(opts, &topo)
}

/// Spawn nodes and stay up until the server publishes `shutdown`.
///
/// Host commands arrive on `ctrl/s2a`. Status is published on `status/a2s`.
pub fn run_arbiter_with_topology(
    opts: &ArbiterOptions,
    topo: &LogicalTopology,
) -> Result<(), ArbiterError> {
    if !opts.node_bin.is_file() {
        return Err(ArbiterError::Message(format!(
            "board binary not found at {} (build rl78-minimal-board or pass --node-bin)",
            opts.node_bin.display()
        )));
    }
    run_host_session(opts, topo)
}

/// Test helper: Ready barrier, immediate Start, then a short time-sync window.
#[cfg(test)]
fn ready_barrier_and_start(
    topo: &LogicalTopology,
    control: &ArbiterControl,
) -> Result<(), ArbiterError> {
    let mut peers = await_ready(topo, control)?;
    control.publish(&ControlToNode::Start)?;
    for state in peers.values_mut() {
        *state = state.on_start_broadcast();
    }
    log::info!("Start broadcast");
    run_time_sync(topo, control, &mut peers)?;
    Ok(())
}

fn await_ready(
    topo: &LogicalTopology,
    control: &ArbiterControl,
) -> Result<HashMap<String, PeerState>, ArbiterError> {
    let timeout = Duration::from_millis(topo.ready_timeout_ms);
    let deadline = Instant::now() + timeout;
    let mut peers: HashMap<String, PeerState> = topo
        .boards
        .iter()
        .map(|b| (b.id.clone(), PeerState::AwaitingReady))
        .collect();

    let startup = ControlToNode::StartupRecord {
        margin_ns: topo.margin_ns,
        headroom_threshold_ns: topo.headroom_threshold_ns,
    };
    control.publish(&startup)?;
    let mut last_startup = Instant::now();

    while peers.values().any(|p| *p != PeerState::Ready) {
        if Instant::now() >= deadline {
            return Err(ready_timeout(topo, &peers));
        }
        if last_startup.elapsed() > Duration::from_millis(50)
            && peers.values().any(|p| *p == PeerState::AwaitingReady)
        {
            let _ = control.publish(&startup);
            last_startup = Instant::now();
        }

        let mut progressed = false;
        while let Some(msg) = control.try_recv()? {
            match msg {
                ControlToArbiter::Ready { from } => {
                    let Some(board_id) = control.resolve_board(from).map(str::to_owned) else {
                        continue;
                    };
                    let Some(state) = peers.get_mut(&board_id) else {
                        continue;
                    };
                    let (next, effect) = state.on_message(msg);
                    warn_peer(&board_id, effect)?;
                    if next != *state {
                        log::trace!("peer '{board_id}' {state:?} -> {next:?}");
                    }
                    *state = next;
                    progressed = true;
                }
                other => {
                    log::warn!("ignoring unexpected before Start: {other:?}");
                }
            }
        }
        if !progressed {
            thread::sleep(ARBITER_POLL_IDLE);
        }
    }

    log::info!("Ready barrier complete ({} nodes)", peers.len());
    Ok(peers)
}

#[cfg(test)]
fn run_time_sync(
    topo: &LogicalTopology,
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
) -> Result<(), ArbiterError> {
    use crate::time_sync::TimeCeiling;

    let mut ceiling = TimeCeiling::new(topo.boards.iter().map(|b| b.id.clone()), topo.margin_ns);
    let mut allowed = ceiling.allowed_ns();
    control.publish(&ControlToNode::Allowed {
        allowed_ns: allowed,
    })?;
    log::info!("initial Allowed={allowed}");

    // TODO(cluster-loop): MVP wall-clock window only. Replace with an open loop
    // until ControlToNode::Shutdown (or equivalent) ends the run.
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        let mut changed = false;
        let mut cluster_stop: Option<(String, HostStopReason)> = None;
        while let Some(msg) = control.try_recv()? {
            let from = msg.from_board();
            let Some(board_id) = control.resolve_board(from).map(str::to_owned) else {
                continue;
            };
            let Some(state) = peers.get_mut(&board_id) else {
                continue;
            };
            let (next, effect) = state.on_message(msg);
            match effect.clone() {
                Some(PeerEffect::TimeReport {
                    from: _,
                    virtual_time_ns,
                }) => {
                    ceiling.report(&board_id, virtual_time_ns);
                    changed = true;
                }
                Some(PeerEffect::HostStop { from: _, reason }) => {
                    // HostStopReason variants are all cluster-relevant by construction.
                    cluster_stop = Some((board_id.clone(), reason));
                }
                other => warn_peer(&board_id, other)?,
            }
            *state = next;
        }
        if let Some((source, reason)) = cluster_stop {
            log::info!("HostStop from '{source}' ({reason}); broadcasting ClusterStop");
            broadcast_cluster_stop(control, peers, &source, reason)?;
            return Ok(());
        }
        if changed {
            let next_allowed = ceiling.allowed_ns();
            if next_allowed != allowed {
                allowed = next_allowed;
                control.publish(&ControlToNode::Allowed {
                    allowed_ns: allowed,
                })?;
            }
        }
        std::thread::sleep(ARBITER_POLL_IDLE);
    }
    Ok(())
}

enum SyncStep {
    Continue,
    Stopped,
}

fn run_host_session(opts: &ArbiterOptions, topo: &LogicalTopology) -> Result<(), ArbiterError> {
    let board_ids: Vec<&str> = topo.boards.iter().map(|b| b.id.as_str()).collect();
    let boards = board_hash_table(board_ids).map_err(ArbiterError::Message)?;

    let config = isolated_config(&opts.iox_root)?;
    let node = create_node(&config, &format!("arbiter-{}", opts.cluster_key))?;
    let host = ArbiterHostPort::open(&node, &opts.cluster_key)?;
    let control =
        ArbiterControl::create(&node, &opts.cluster_key, topo.boards.len(), boards.clone())?;
    let mut logs =
        ArbiterLogBus::create(&opts.iox_root, &opts.cluster_key, topo.boards.len(), boards)?;
    log::info!(
        "loaded topology ({} boards, {} edges, margin_ns={}, cluster_key={})",
        topo.boards.len(),
        topo.edges.len(),
        topo.margin_ns,
        opts.cluster_key
    );

    let mut children = spawn_nodes(opts, topo)?;
    let mut peers = match await_ready(topo, &control) {
        Ok(peers) => peers,
        Err(err) => {
            stop_children(&mut children);
            logs.shutdown();
            return Err(err);
        }
    };
    let mut ceiling = TimeCeiling::new(topo.boards.iter().map(|b| b.id.clone()), topo.margin_ns);
    let mut allowed = ceiling.allowed_ns();
    let mut running = false;
    let mut last_status = Instant::now();
    write_status(&host, running, ceiling.virtual_time_ns(), topo, &peers)?;
    let mut reported_vt = ceiling.virtual_time_ns();
    let mut reported_running = running;

    let result = loop {
        let mut shutdown = false;
        loop {
            match host.try_cmd()? {
                Some(HostCommand::Shutdown) => {
                    shutdown = true;
                    break;
                }
                Some(HostCommand::Start) if !running => {
                    begin_running(&control, &mut peers, allowed)?;
                    running = true;
                }
                Some(HostCommand::Stop) if running => {
                    log::info!("host Stop");
                    broadcast_cluster_stop(
                        &control,
                        &mut peers,
                        "server",
                        HostStopReason::ExternalStop,
                    )?;
                    running = false;
                }
                Some(HostCommand::Reset) => {
                    log::info!("host Reset");
                    broadcast_cluster_stop(
                        &control,
                        &mut peers,
                        "server",
                        HostStopReason::ExternalStop,
                    )?;

                    control.publish(&ControlToNode::Reset)?;
                    broadcast_start(&control, &mut peers)?;
                    running = true;
                    log::info!("host Reset: Start");
                }
                Some(HostCommand::Start) | Some(HostCommand::Stop) => {}
                None => break,
            }
        }

        if shutdown {
            break Ok(());
        }

        match step_time_sync(&control, &mut peers, &mut ceiling, &mut allowed)? {
            SyncStep::Continue => {}
            SyncStep::Stopped => {
                running = false;
            }
        }

        let vt = ceiling.virtual_time_ns();
        if running != reported_running
            || vt != reported_vt
            || last_status.elapsed() >= Duration::from_millis(100)
        {
            write_status(&host, running, vt, topo, &peers)?;
            reported_running = running;
            reported_vt = vt;
            last_status = Instant::now();
        }
    };

    logs.shutdown();
    stop_children(&mut children);
    result
}

fn begin_running(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    allowed: u64,
) -> Result<(), ArbiterError> {
    broadcast_start(control, peers)?;
    control.publish(&ControlToNode::Allowed {
        allowed_ns: allowed,
    })?;
    Ok(())
}

fn broadcast_start(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
) -> Result<(), ArbiterError> {
    control.publish(&ControlToNode::Start)?;
    for state in peers.values_mut() {
        *state = state.on_start_broadcast();
    }
    log::info!("Start broadcast");
    Ok(())
}

fn step_time_sync(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    ceiling: &mut TimeCeiling,
    allowed: &mut u64,
) -> Result<SyncStep, ArbiterError> {
    let mut changed = false;
    let mut cluster_stop: Option<(String, HostStopReason)> = None;
    while let Some(msg) = control.try_recv()? {
        let from = msg.from_board();
        let Some(board_id) = control.resolve_board(from).map(str::to_owned) else {
            continue;
        };
        let Some(state) = peers.get_mut(&board_id) else {
            continue;
        };
        let (next, effect) = state.on_message(msg);
        match effect.clone() {
            Some(PeerEffect::TimeReport {
                from: _,
                virtual_time_ns,
            }) => {
                ceiling.report(&board_id, virtual_time_ns);
                changed = true;
            }
            Some(PeerEffect::HostStop { from: _, reason }) => {
                cluster_stop = Some((board_id.clone(), reason));
            }
            other => warn_peer(&board_id, other)?,
        }
        *state = next;
    }
    if let Some((source, reason)) = cluster_stop {
        log::info!("HostStop from '{source}' ({reason}); broadcasting ClusterStop");
        broadcast_cluster_stop(control, peers, &source, reason)?;
        return Ok(SyncStep::Stopped);
    }
    if changed {
        let next_allowed = ceiling.allowed_ns();
        if next_allowed != *allowed {
            *allowed = next_allowed;
            control.publish(&ControlToNode::Allowed {
                allowed_ns: *allowed,
            })?;
        }
    }
    thread::sleep(ARBITER_POLL_IDLE);
    Ok(SyncStep::Continue)
}

fn spawn_nodes(opts: &ArbiterOptions, topo: &LogicalTopology) -> Result<Vec<Child>, ArbiterError> {
    let mut children = Vec::new();
    for board in &topo.boards {
        let child = Command::new(&opts.node_bin)
            .arg("--board-id")
            .arg(&board.id)
            .arg("--cluster-key")
            .arg(&opts.cluster_key)
            .arg("--iox-root")
            .arg(&opts.iox_root)
            .arg("--topology")
            .arg(&opts.topology_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        children.push(child);
    }
    Ok(children)
}

fn stop_children(children: &mut Vec<Child>) {
    for child in children.iter_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    children.clear();
}

fn write_status(
    host: &ArbiterHostPort,
    running: bool,
    virtual_time_ns: u64,
    topo: &LogicalTopology,
    peers: &HashMap<String, PeerState>,
) -> Result<(), ArbiterError> {
    let nodes = topo
        .boards
        .iter()
        .map(|board| SessionNode {
            id: board.id.clone(),
            kind: board.kind.clone(),
            state: peers
                .get(&board.id)
                .map(peer_state_name)
                .unwrap_or("unknown")
                .to_string(),
        })
        .collect();
    let status = SessionStatus {
        state: if running { "running" } else { "stopped" }.to_string(),
        virtual_time_ns,
        nodes,
    };
    host.publish_status(&status)?;
    Ok(())
}

fn peer_state_name(state: &PeerState) -> &'static str {
    match state {
        PeerState::AwaitingReady => "awaiting_ready",
        PeerState::Ready => "ready",
        PeerState::Running => "running",
        PeerState::Stopped => "stopped",
    }
}

fn broadcast_cluster_stop(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    _source: &str,
    reason: HostStopReason,
) -> Result<(), ArbiterError> {
    control.publish(&ControlToNode::ClusterStop { reason })?;
    for state in peers.values_mut() {
        *state = PeerState::Stopped;
    }
    Ok(())
}

fn ready_timeout(topo: &LogicalTopology, peers: &HashMap<String, PeerState>) -> ArbiterError {
    let missing = peers
        .iter()
        .filter(|(_, p)| **p != PeerState::Ready)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>()
        .join(",");
    ArbiterError::ReadyTimeout {
        timeout_ms: topo.ready_timeout_ms,
        missing,
    }
}

fn warn_peer(board_id: &str, effect: Option<PeerEffect>) -> Result<(), ArbiterError> {
    match effect {
        Some(PeerEffect::Warn(msg)) => {
            log::warn!("warning [{board_id}]: {msg}");
        }
        Some(PeerEffect::TimeReport { .. }) | Some(PeerEffect::HostStop { .. }) | None => {}
    }

    Ok(())
}

/// Convenience for the binary: topology path only.
pub fn run_arbiter_from_path(topology_path: &Path) -> Result<(), ArbiterError> {
    let opts = ArbiterOptions::from_topology_path(topology_path.to_path_buf())?;
    run_arbiter(&opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{
        ArbiterControl, NodeControl, board_hash_table, board_id_hash, create_node, isolated_config,
    };
    use crate::topology::{BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, PayloadKind};
    use serial_test::serial;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn two_board_topo(ready_timeout_ms: u64) -> LogicalTopology {
        LogicalTopology {
            boards: vec![
                BoardSpec {
                    id: "a".into(),
                    kind: "rl78".into(),
                    elf: None,
                    endpoints: vec![
                        EndpointSpec {
                            name: "uart0_tx".into(),
                            direction: EndpointDirection::Out,
                            payload: PayloadKind::Uart,
                        },
                        EndpointSpec {
                            name: "uart0_rx".into(),
                            direction: EndpointDirection::In,
                            payload: PayloadKind::Uart,
                        },
                    ],
                },
                BoardSpec {
                    id: "b".into(),
                    kind: "rl78".into(),
                    elf: None,
                    endpoints: vec![
                        EndpointSpec {
                            name: "uart0_tx".into(),
                            direction: EndpointDirection::Out,
                            payload: PayloadKind::Uart,
                        },
                        EndpointSpec {
                            name: "uart0_rx".into(),
                            direction: EndpointDirection::In,
                            payload: PayloadKind::Uart,
                        },
                    ],
                },
            ],
            edges: vec![
                DirectedEdge {
                    from_board: "a".into(),
                    from_endpoint: "uart0_tx".into(),
                    to_board: "b".into(),
                    to_endpoint: "uart0_rx".into(),
                    payload: PayloadKind::Uart,
                    uart_ring_len: None,
                },
                DirectedEdge {
                    from_board: "b".into(),
                    from_endpoint: "uart0_tx".into(),
                    to_board: "a".into(),
                    to_endpoint: "uart0_rx".into(),
                    payload: PayloadKind::Uart,
                    uart_ring_len: None,
                },
            ],
            margin_ns: 1000,
            headroom_threshold_ns: 100,
            ready_timeout_ms,
        }
    }

    fn fake_node_loop(
        cluster_key: String,
        iox_root: PathBuf,
        board_id: String,
        send_ready: bool,
        inject_host_stop: bool,
        saw_cluster_stop: Option<Arc<Mutex<bool>>>,
    ) {
        let topo = two_board_topo(2_000);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, &format!("fake-{board_id}-{cluster_key}")).unwrap();
        let control = NodeControl::open(&node, &cluster_key, boards).unwrap();
        // UART attach is covered by dedicated tests; skip here to isolate control timing.

        let my_hash = board_id_hash(&board_id);
        let end = Instant::now() + Duration::from_secs(3);
        let mut got_start = false;
        while Instant::now() < end {
            while let Ok(Some(msg)) = control.try_recv() {
                match msg {
                    ControlToNode::StartupRecord { .. } => {
                        if send_ready {
                            let _ = control.publish(&ControlToArbiter::Ready { from: my_hash });
                            let _ = control.publish(&ControlToArbiter::Ready { from: my_hash });
                        }
                    }
                    ControlToNode::Start => {
                        got_start = true;
                        if inject_host_stop {
                            let _ = control.publish(&ControlToArbiter::HostStop {
                                from: my_hash,
                                reason: HostStopReason::Breakpoint,
                            });
                        }
                    }
                    ControlToNode::ClusterStop { reason } => {
                        assert_eq!(reason, HostStopReason::Breakpoint);
                        if let Some(flag) = &saw_cluster_stop {
                            *flag.lock().unwrap() = true;
                        }
                        return;
                    }
                    _ => {}
                }
            }
            if got_start && !inject_host_stop && saw_cluster_stop.is_none() {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    #[serial]
    fn ready_barrier_succeeds_with_fake_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-ready-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let topo = two_board_topo(2_000);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, "arb-test-ready").unwrap();
        let _logs =
            ArbiterLogBus::create(&iox_root, &cluster_key, topo.boards.len(), boards.clone())
                .unwrap();
        let control =
            ArbiterControl::create(&node, &cluster_key, topo.boards.len(), boards).unwrap();

        let mut joins = Vec::new();
        for board in &topo.boards {
            let ck = cluster_key.clone();
            let root = iox_root.clone();
            let id = board.id.clone();
            joins.push(thread::spawn(move || {
                fake_node_loop(ck, root, id, true, false, None);
            }));
        }

        ready_barrier_and_start(&topo, &control).unwrap();
        for j in joins {
            j.join().unwrap();
        }
    }

    #[test]
    #[serial]
    fn ready_barrier_times_out_when_node_silent() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-timeout-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let topo = two_board_topo(200);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, "arb-test-timeout").unwrap();
        let _logs =
            ArbiterLogBus::create(&iox_root, &cluster_key, topo.boards.len(), boards.clone())
                .unwrap();
        let control =
            ArbiterControl::create(&node, &cluster_key, topo.boards.len(), boards).unwrap();

        let mut joins = Vec::new();
        for (i, board) in topo.boards.iter().enumerate() {
            let ck = cluster_key.clone();
            let root = iox_root.clone();
            let id = board.id.clone();
            let send = i == 0;
            joins.push(thread::spawn(move || {
                fake_node_loop(ck, root, id, send, false, None);
            }));
        }

        let err = ready_barrier_and_start(&topo, &control).unwrap_err();
        assert!(matches!(err, ArbiterError::ReadyTimeout { .. }));
        for j in joins {
            let _ = j.join();
        }
    }

    #[test]
    #[serial]
    fn host_stop_broadcasts_cluster_stop() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-hoststop-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let topo = two_board_topo(2_000);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, "arb-test-hoststop").unwrap();
        let _logs =
            ArbiterLogBus::create(&iox_root, &cluster_key, topo.boards.len(), boards.clone())
                .unwrap();
        let control =
            ArbiterControl::create(&node, &cluster_key, topo.boards.len(), boards).unwrap();

        let saw_cluster_stop = Arc::new(Mutex::new(false));
        let mut joins = Vec::new();
        for board in &topo.boards {
            let ck = cluster_key.clone();
            let root = iox_root.clone();
            let id = board.id.clone();
            let inject = id == "a";
            let flag = if inject {
                None
            } else {
                Some(Arc::clone(&saw_cluster_stop))
            };
            joins.push(thread::spawn(move || {
                fake_node_loop(ck, root, id, true, inject, flag);
            }));
        }

        ready_barrier_and_start(&topo, &control).unwrap();
        for j in joins {
            j.join().unwrap();
        }
        assert!(
            *saw_cluster_stop.lock().unwrap(),
            "peer should receive ClusterStop after HostStop"
        );
    }

    #[test]
    fn duplicate_service_create_fails() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-dup-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let boards = board_hash_table(["a", "b"]).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node1 = create_node(&config, "arb-dup-1").unwrap();
        let _c1 = ArbiterControl::create(&node1, &cluster_key, 2, boards.clone()).unwrap();
        let node2 = create_node(&config, "arb-dup-2").unwrap();
        let err = ArbiterControl::create(&node2, &cluster_key, 2, boards);
        assert!(
            err.is_err(),
            "expected duplicate create to fail, got Ok(...)"
        );
        let err = err.err().unwrap();
        assert!(
            err.to_string().contains("create"),
            "expected create failure, got {err}"
        );
    }
}
