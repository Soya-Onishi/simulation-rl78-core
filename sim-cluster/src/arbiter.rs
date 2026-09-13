//! Arbiter: create iceoryx services, spawn nodes, Ready barrier, Start.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::control::ControlMessage;
use crate::ipc::{
    board_hash_table, create_node, isolated_config, new_cluster_key, root_path_for_cluster,
    ArbiterControl, IpcError,
};
use crate::lifecycle::{PeerEffect, PeerState};
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
            node_bin: exe_dir.join("cluster-node"),
            cluster_key,
            iox_root,
        })
    }
}

/// Load topology, spawn nodes, wait for Ready, broadcast Start.
pub fn run_arbiter(opts: &ArbiterOptions) -> Result<(), ArbiterError> {
    let text = fs::read_to_string(&opts.topology_path)?;
    let topo = LogicalTopology::from_json_str(&text)?;
    run_arbiter_with_topology(opts, &topo)
}

/// Core arbiter loop (also used from tests with an in-memory topology).
pub fn run_arbiter_with_topology(
    opts: &ArbiterOptions,
    topo: &LogicalTopology,
) -> Result<(), ArbiterError> {
    eprintln!(
        "cluster-arbiter: loaded topology ({} boards, {} edges, margin_ns={}, cluster_key={})",
        topo.boards.len(),
        topo.edges.len(),
        topo.margin_ns,
        opts.cluster_key
    );

    if !opts.node_bin.is_file() {
        return Err(ArbiterError::Message(format!(
            "cluster-node not found at {}",
            opts.node_bin.display()
        )));
    }

    let board_ids: Vec<&str> = topo.boards.iter().map(|b| b.id.as_str()).collect();
    let boards = board_hash_table(board_ids).map_err(ArbiterError::Message)?;

    let config = isolated_config(&opts.iox_root)?;
    let node = create_node(&config, &format!("arbiter-{}", opts.cluster_key))?;
    let control = ArbiterControl::create(&node, &opts.cluster_key, topo.boards.len(), boards)?;
    // UART services are open_or_create'd by the TX/RX nodes (topology QoS).

    let mut children: Vec<Child> = Vec::new();
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
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;
        children.push(child);
    }

    let result = ready_barrier_and_start(topo, &control);
    if result.is_ok() {
        std::thread::sleep(Duration::from_millis(50));
    }

    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = fs::remove_dir_all(&opts.iox_root);
    result
}

fn ready_barrier_and_start(
    topo: &LogicalTopology,
    control: &ArbiterControl,
) -> Result<(), ArbiterError> {
    let timeout = Duration::from_millis(topo.ready_timeout_ms);
    let deadline = Instant::now() + timeout;
    let mut peers: HashMap<String, PeerState> = topo
        .boards
        .iter()
        .map(|b| (b.id.clone(), PeerState::AwaitingReady))
        .collect();

    // Publish StartupRecord for every board (nodes may still be opening).
    for board in &topo.boards {
        control.publish(&ControlMessage::StartupRecord {
            board_id: board.id.clone(),
            margin_ns: topo.margin_ns,
            headroom_threshold_ns: topo.headroom_threshold_ns,
        })?;
    }
    // Re-publish periodically until Ready so late openers still see Startup.
    let mut last_startup = Instant::now();

    while peers.values().any(|p| *p != PeerState::Ready) {
        if Instant::now() >= deadline {
            return ready_timeout(topo, &peers);
        }
        if last_startup.elapsed() > Duration::from_millis(50) {
            for board in &topo.boards {
                if peers.get(&board.id) == Some(&PeerState::AwaitingReady) {
                    let _ = control.publish(&ControlMessage::StartupRecord {
                        board_id: board.id.clone(),
                        margin_ns: topo.margin_ns,
                        headroom_threshold_ns: topo.headroom_threshold_ns,
                    });
                }
            }
            last_startup = Instant::now();
        }

        let mut progressed = false;
        while let Some(msg) = control.try_recv()? {
            match &msg {
                ControlMessage::Ready { board_id } => {
                    let Some(state) = peers.get_mut(board_id) else {
                        continue;
                    };
                    let (next, effect) = state.on_message(board_id, msg.clone());
                    warn_peer(board_id, effect);
                    if next != *state {
                        eprintln!("cluster-arbiter: peer '{board_id}' {state:?} -> {next:?}");
                    }
                    *state = next;
                    progressed = true;
                }
                other => {
                    eprintln!("cluster-arbiter: ignoring unexpected before Start: {other:?}");
                }
            }
        }
        if !progressed {
            std::thread::sleep(ARBITER_POLL_IDLE);
        }
    }

    eprintln!(
        "cluster-arbiter: Ready barrier complete ({} nodes)",
        peers.len()
    );

    control.publish(&ControlMessage::Start)?;
    for state in peers.values_mut() {
        *state = state.on_start_broadcast();
    }
    eprintln!("cluster-arbiter: Start broadcast");

    run_time_sync(topo, control, &mut peers)?;
    Ok(())
}

fn run_time_sync(
    topo: &LogicalTopology,
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
) -> Result<(), ArbiterError> {
    use crate::cluster_stop::is_cluster_relevant_reason;
    use crate::time_sync::TimeCeiling;

    let mut ceiling = TimeCeiling::new(topo.boards.iter().map(|b| b.id.clone()), topo.margin_ns);
    let mut allowed = ceiling.allowed_ns();
    control.publish(&ControlMessage::Allowed {
        allowed_ns: allowed,
    })?;
    eprintln!("cluster-arbiter: initial Allowed={allowed}");

    // TODO(cluster-loop): MVP wall-clock window only. Replace
    // `while Instant::now() < deadline` with an open `loop` that keeps
    // receiving (TimeReport / HostStop) and can broadcast ClusterStop until
    // an explicit shutdown condition; otherwise post-Start control is dead.
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        let mut changed = false;
        let mut cluster_stop: Option<(String, String)> = None;
        while let Some(msg) = control.try_recv()? {
            let board_id = match &msg {
                ControlMessage::TimeReport { board_id, .. }
                | ControlMessage::HostStop { board_id, .. }
                | ControlMessage::Ready { board_id } => board_id.clone(),
                _ => {
                    eprintln!("cluster-arbiter: ignoring {msg:?} in time-sync");
                    continue;
                }
            };
            let Some(state) = peers.get_mut(&board_id) else {
                continue;
            };
            let (next, effect) = state.on_message(&board_id, msg);
            match effect.clone() {
                Some(PeerEffect::TimeReport {
                    board_id: id,
                    virtual_time_ns,
                }) => {
                    ceiling.report(&id, virtual_time_ns);
                    changed = true;
                }
                Some(PeerEffect::HostStop {
                    board_id: id,
                    reason,
                }) => {
                    if is_cluster_relevant_reason(&reason) {
                        cluster_stop = Some((id, reason));
                    } else {
                        eprintln!(
                            "cluster-arbiter: ignoring non-cluster HostStop from '{id}': {reason}"
                        );
                    }
                }
                other => warn_peer(&board_id, other),
            }
            *state = next;
        }
        if let Some((source, reason)) = cluster_stop {
            eprintln!(
                "cluster-arbiter: HostStop from '{source}' ({reason}); broadcasting ClusterStop"
            );
            broadcast_cluster_stop(control, peers, &source, &reason)?;
            return Ok(());
        }
        if changed {
            let next_allowed = ceiling.allowed_ns();
            if next_allowed != allowed {
                allowed = next_allowed;
                control.publish(&ControlMessage::Allowed {
                    allowed_ns: allowed,
                })?;
                eprintln!("cluster-arbiter: Allowed={allowed}");
            }
        }
        std::thread::sleep(ARBITER_POLL_IDLE);
    }
    Ok(())
}

fn broadcast_cluster_stop(
    control: &ArbiterControl,
    peers: &mut HashMap<String, PeerState>,
    source: &str,
    reason: &str,
) -> Result<(), ArbiterError> {
    control.publish(&ControlMessage::ClusterStop {
        reason: reason.to_string(),
    })?;
    for (board_id, state) in peers.iter_mut() {
        if board_id == source {
            *state = PeerState::Stopped;
            continue;
        }
        *state = PeerState::Stopped;
    }
    Ok(())
}

fn ready_timeout(
    topo: &LogicalTopology,
    peers: &HashMap<String, PeerState>,
) -> Result<(), ArbiterError> {
    let missing = peers
        .iter()
        .filter(|(_, p)| **p != PeerState::Ready)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>()
        .join(",");
    Err(ArbiterError::ReadyTimeout {
        timeout_ms: topo.ready_timeout_ms,
        missing,
    })
}

fn warn_peer(board_id: &str, effect: Option<PeerEffect>) {
    match effect {
        Some(PeerEffect::Warn(msg)) => {
            eprintln!("cluster-arbiter: warning [{board_id}]: {msg}");
        }
        Some(PeerEffect::TimeReport { .. }) | Some(PeerEffect::HostStop { .. }) | None => {}
    }
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
        board_hash_table, create_node, isolated_config, ArbiterControl, NodeControl,
    };
    use crate::topology::{BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, PayloadKind};
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
        let control =
            NodeControl::open(&node, &cluster_key, boards).unwrap();
        // UART attach is covered by dedicated tests; skip here to isolate control timing.

        let end = Instant::now() + Duration::from_secs(3);
        let mut got_start = false;
        while Instant::now() < end {
            while let Ok(Some(msg)) = control.try_recv() {
                match msg {
                    ControlMessage::StartupRecord { board_id: id, .. } if id == board_id => {
                        if send_ready {
                            let _ = control.publish(&ControlMessage::Ready {
                                board_id: board_id.clone(),
                            });
                            let _ = control.publish(&ControlMessage::Ready {
                                board_id: board_id.clone(),
                            });
                        }
                    }
                    ControlMessage::Start => {
                        got_start = true;
                        if inject_host_stop {
                            let _ = control.publish(&ControlMessage::HostStop {
                                board_id: board_id.clone(),
                                reason: "breakpoint".into(),
                            });
                        }
                    }
                    ControlMessage::ClusterStop { reason } => {
                        assert_eq!(reason, "breakpoint");
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
    fn ready_barrier_succeeds_with_fake_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-ready-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let topo = two_board_topo(2_000);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, "arb-test-ready").unwrap();
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
    fn ready_barrier_times_out_when_node_silent() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-timeout-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let topo = two_board_topo(200);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, "arb-test-timeout").unwrap();
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
    fn host_stop_broadcasts_cluster_stop() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_key = format!("t-hoststop-{}", std::process::id());
        let iox_root = dir.path().join("iox");
        let topo = two_board_topo(2_000);
        let boards = board_hash_table(topo.boards.iter().map(|b| b.id.as_str())).unwrap();
        let config = isolated_config(&iox_root).unwrap();
        let node = create_node(&config, "arb-test-hoststop").unwrap();
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
