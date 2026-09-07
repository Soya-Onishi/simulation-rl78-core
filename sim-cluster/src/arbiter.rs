//! Arbiter: bind control resources, spawn nodes, Ready barrier, Start.
//!
//! Per-peer progress is an event-driven [`PeerState`] machine. Unexpected
//! messages warn and are ignored; only Ready-barrier timeout is fatal for
//! starting the simulation.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::control::{
    ControlMessage, ShmBinding, ShmRole, accept_timeout, bind_listener, default_control_dir,
    read_message_timeout, write_message,
};
use crate::lifecycle::{PeerEffect, PeerState};
use crate::shm_uart::{ShmUartError, UartShmOwner};
use crate::topology::{DirectedEdge, LogicalTopology, PayloadKind, TopologyError};

/// Arbiter failures.
#[derive(Debug, thiserror::Error)]
pub enum ArbiterError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("topology error: {0}")]
    Topology(#[from] TopologyError),
    #[error("ready barrier timed out after {timeout_ms}ms; missing boards: {missing}")]
    ReadyTimeout { timeout_ms: u64, missing: String },
    #[error("shm error: {0}")]
    Shm(#[from] ShmUartError),
    #[error("{0}")]
    Message(String),
}

/// Options for [`run_arbiter`].
#[derive(Clone, Debug)]
pub struct ArbiterOptions {
    pub topology_path: PathBuf,
    pub node_bin: PathBuf,
    /// Directory for per-board control sockets.
    pub control_dir: PathBuf,
}

impl ArbiterOptions {
    pub fn from_topology_path(topology_path: PathBuf) -> Result<Self, ArbiterError> {
        let exe = std::env::current_exe().map_err(ArbiterError::Io)?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| ArbiterError::Message("current_exe has no parent".into()))?;
        Ok(Self {
            topology_path,
            node_bin: exe_dir.join("cluster-node"),
            control_dir: default_control_dir().join(format!("arb-{}", std::process::id())),
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
        "cluster-arbiter: loaded topology ({} boards, {} edges, margin_ns={})",
        topo.boards.len(),
        topo.edges.len(),
        topo.margin_ns
    );

    if !opts.node_bin.is_file() {
        return Err(ArbiterError::Message(format!(
            "cluster-node not found at {}",
            opts.node_bin.display()
        )));
    }

    fs::create_dir_all(&opts.control_dir)?;

    let edge_bindings = create_edge_shm(topo, &opts.control_dir)?;
    // Keep owners alive for the cluster lifetime (Drop unlinks flinks).
    let _shm_owners: Vec<UartShmOwner> = edge_bindings.owners;

    let mut children: Vec<Child> = Vec::new();
    let mut listeners = Vec::new();

    for board in &topo.boards {
        let sock = opts.control_dir.join(format!("{}.sock", board.id));
        let listener = bind_listener(&sock)?;
        listeners.push((board.id.clone(), sock.clone(), listener));

        let child = Command::new(&opts.node_bin)
            .arg("--board-id")
            .arg(&board.id)
            .arg("--control")
            .arg(&sock)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;
        children.push(child);
    }

    let result = ready_barrier_and_start(topo, &mut listeners, &edge_bindings.by_board);
    if result.is_ok() {
        // Phase 2: Start delivered; leave nodes running briefly then stop.
        // Later phases keep the arbiter event loop alive.
        std::thread::sleep(Duration::from_millis(50));
    }

    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = fs::remove_dir_all(&opts.control_dir);
    result
}

struct PeerSlot {
    state: PeerState,
    stream: Option<std::os::unix::net::UnixStream>,
}

struct EdgeShmBundle {
    owners: Vec<UartShmOwner>,
    by_board: HashMap<String, Vec<ShmBinding>>,
}

fn edge_id(edge: &DirectedEdge) -> String {
    format!(
        "{}:{}->{}:{}",
        edge.from_board, edge.from_endpoint, edge.to_board, edge.to_endpoint
    )
}

fn create_edge_shm(
    topo: &LogicalTopology,
    control_dir: &Path,
) -> Result<EdgeShmBundle, ArbiterError> {
    let mut owners = Vec::new();
    let mut by_board: HashMap<String, Vec<ShmBinding>> = HashMap::new();
    for (i, edge) in topo.edges.iter().enumerate() {
        if edge.payload != PayloadKind::Uart {
            continue;
        }
        let id = edge_id(edge);
        let flink = control_dir.join(format!("uart-edge-{i}.shm"));
        let capacity = LogicalTopology::uart_ring_len(edge);
        let owner = UartShmOwner::create(&flink, capacity)?;
        let flink_name = owner.flink().display().to_string();
        by_board
            .entry(edge.from_board.clone())
            .or_default()
            .push(ShmBinding {
                edge_id: id.clone(),
                flink_name: flink_name.clone(),
                role: ShmRole::Producer,
            });
        by_board
            .entry(edge.to_board.clone())
            .or_default()
            .push(ShmBinding {
                edge_id: id,
                flink_name,
                role: ShmRole::Consumer,
            });
        owners.push(owner);
    }
    Ok(EdgeShmBundle { owners, by_board })
}

fn ready_barrier_and_start(
    topo: &LogicalTopology,
    listeners: &mut [(String, PathBuf, std::os::unix::net::UnixListener)],
    bindings_by_board: &HashMap<String, Vec<ShmBinding>>,
) -> Result<(), ArbiterError> {
    let timeout = Duration::from_millis(topo.ready_timeout_ms);
    let deadline = Instant::now() + timeout;
    let expected: Vec<String> = topo.boards.iter().map(|b| b.id.clone()).collect();
    let mut peers: HashMap<String, PeerSlot> = expected
        .iter()
        .map(|id| {
            (
                id.clone(),
                PeerSlot {
                    state: PeerState::Accepting,
                    stream: None,
                },
            )
        })
        .collect();

    // Accept connections (event: connect) until every peer is AwaitingReady or timeout.
    while peers.values().any(|p| p.state == PeerState::Accepting) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return ready_timeout(topo, &peers);
        }
        let mut progressed = false;
        for (board_id, _sock, listener) in listeners.iter() {
            let Some(slot) = peers.get_mut(board_id) else {
                continue;
            };
            if slot.state != PeerState::Accepting {
                continue;
            }
            let slice = remaining.min(Duration::from_millis(20));
            if let Some(mut stream) = accept_timeout(listener, slice)? {
                let (next, effect) = slot.state.on_connected();
                warn_peer(board_id, effect);
                let shm_segments = bindings_by_board.get(board_id).cloned().unwrap_or_default();
                let startup = ControlMessage::StartupRecord {
                    board_id: board_id.clone(),
                    shm_segments,
                    margin_ns: topo.margin_ns,
                    headroom_threshold_ns: topo.headroom_threshold_ns,
                };
                write_message(&mut stream, &startup)?;
                slot.state = next;
                slot.stream = Some(stream);
                progressed = true;
            }
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // Collect Ready events until all peers are Ready or timeout.
    while peers.values().any(|p| p.state != PeerState::Ready) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return ready_timeout(topo, &peers);
        }

        let mut progressed = false;
        for (board_id, slot) in peers.iter_mut() {
            if slot.state == PeerState::Ready {
                continue;
            }
            let Some(stream) = slot.stream.as_mut() else {
                continue;
            };
            let slice = remaining.min(Duration::from_millis(50));
            match read_message_timeout(stream, slice) {
                Ok(msg) => {
                    let (next, effect) = slot.state.on_message(board_id, msg);
                    warn_peer(board_id, effect);
                    if next != slot.state {
                        eprintln!(
                            "cluster-arbiter: peer '{board_id}' {:?} -> {next:?}",
                            slot.state
                        );
                    }
                    slot.state = next;
                    progressed = true;
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::TimedOut
                        || err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(ArbiterError::Io(err)),
            }
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    eprintln!(
        "cluster-arbiter: Ready barrier complete ({} nodes)",
        peers.len()
    );

    for (board_id, slot) in peers.iter_mut() {
        if let Some(stream) = slot.stream.as_mut() {
            write_message(stream, &ControlMessage::Start)?;
        }
        slot.state = slot.state.on_start_broadcast();
        eprintln!("cluster-arbiter: peer '{board_id}' -> {:?}", slot.state);
    }
    eprintln!("cluster-arbiter: Start broadcast");

    run_time_sync(topo, &mut peers)?;
    Ok(())
}

fn run_time_sync(
    topo: &LogicalTopology,
    peers: &mut HashMap<String, PeerSlot>,
) -> Result<(), ArbiterError> {
    use crate::time_sync::TimeCeiling;

    let mut ceiling = TimeCeiling::new(topo.boards.iter().map(|b| b.id.clone()), topo.margin_ns);
    let mut allowed = ceiling.allowed_ns();
    for slot in peers.values_mut() {
        if let Some(stream) = slot.stream.as_mut() {
            if let Err(err) = write_message(
                stream,
                &ControlMessage::Allowed {
                    allowed_ns: allowed,
                },
            ) {
                eprintln!("cluster-arbiter: warning: Allowed send failed: {err}");
            }
        }
    }
    eprintln!("cluster-arbiter: initial Allowed={allowed}");

    // Brief sync window before Phase 2 teardown; later phases keep this loop.
    let deadline = Instant::now() + Duration::from_millis(150);
    while Instant::now() < deadline {
        let mut changed = false;
        for (board_id, slot) in peers.iter_mut() {
            let Some(stream) = slot.stream.as_mut() else {
                continue;
            };
            match read_message_timeout(stream, Duration::from_millis(10)) {
                Ok(msg) => {
                    let (next, effect) = slot.state.on_message(board_id, msg);
                    if let Some(PeerEffect::TimeReport {
                        board_id: id,
                        virtual_time_ns,
                    }) = effect.clone()
                    {
                        ceiling.report(&id, virtual_time_ns);
                        changed = true;
                    } else {
                        warn_peer(board_id, effect);
                    }
                    slot.state = next;
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::TimedOut
                        || err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    // Peer gone; keep last report in the ceiling.
                }
            }
        }
        if changed {
            let next_allowed = ceiling.allowed_ns();
            if next_allowed != allowed {
                allowed = next_allowed;
                for slot in peers.values_mut() {
                    if let Some(stream) = slot.stream.as_mut() {
                        if let Err(err) = write_message(
                            stream,
                            &ControlMessage::Allowed {
                                allowed_ns: allowed,
                            },
                        ) {
                            eprintln!("cluster-arbiter: warning: Allowed send failed: {err}");
                        }
                    }
                }
                eprintln!("cluster-arbiter: Allowed={allowed}");
            }
        }
    }
    Ok(())
}

fn ready_timeout(
    topo: &LogicalTopology,
    peers: &HashMap<String, PeerSlot>,
) -> Result<(), ArbiterError> {
    let missing = peers
        .iter()
        .filter(|(_, p)| p.state != PeerState::Ready)
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
        Some(PeerEffect::TimeReport { .. }) => {}
        None => {}
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
    use crate::topology::{BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, PayloadKind};
    use std::os::unix::net::UnixStream;
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

    fn fake_node(sock: PathBuf, board_id: String, send_ready: bool) {
        let mut stream = loop {
            match UnixStream::connect(&sock) {
                Ok(s) => break s,
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        };
        let startup = crate::control::read_message(&mut stream).unwrap();
        match startup {
            ControlMessage::StartupRecord { board_id: id, .. } => {
                assert_eq!(id, board_id);
            }
            other => panic!("unexpected {other:?}"),
        }
        if send_ready {
            // Duplicate Ready should be warned, not fatal.
            write_message(
                &mut stream,
                &ControlMessage::Ready {
                    board_id: board_id.clone(),
                },
            )
            .unwrap();
            write_message(
                &mut stream,
                &ControlMessage::Ready {
                    board_id: board_id.clone(),
                },
            )
            .unwrap();
            let start = crate::control::read_message(&mut stream).unwrap();
            assert!(matches!(start, ControlMessage::Start));
            // Stay connected through the arbiter's short time-sync window.
            let end = Instant::now() + Duration::from_millis(250);
            while Instant::now() < end {
                let _ = read_message_timeout(&mut stream, Duration::from_millis(20));
            }
        } else {
            let _ = crate::control::read_message(&mut stream);
        }
    }

    #[test]
    fn ready_barrier_succeeds_with_fake_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let topo = two_board_topo(2_000);
        let mut listeners = Vec::new();
        let mut joins = Vec::new();
        for board in &topo.boards {
            let sock = dir.path().join(format!("{}.sock", board.id));
            let listener = bind_listener(&sock).unwrap();
            let sock2 = sock.clone();
            let id = board.id.clone();
            joins.push(thread::spawn(move || fake_node(sock2, id, true)));
            listeners.push((board.id.clone(), sock, listener));
        }
        ready_barrier_and_start(&topo, &mut listeners, &HashMap::new()).unwrap();
        for j in joins {
            j.join().unwrap();
        }
    }

    #[test]
    fn ready_barrier_times_out_when_node_silent() {
        let dir = tempfile::tempdir().unwrap();
        let topo = two_board_topo(200);
        let mut listeners = Vec::new();
        let mut joins = Vec::new();
        for (i, board) in topo.boards.iter().enumerate() {
            let sock = dir.path().join(format!("{}.sock", board.id));
            let listener = bind_listener(&sock).unwrap();
            let sock2 = sock.clone();
            let id = board.id.clone();
            let send = i == 0;
            joins.push(thread::spawn(move || fake_node(sock2, id, send)));
            listeners.push((board.id.clone(), sock, listener));
        }
        let err = ready_barrier_and_start(&topo, &mut listeners, &HashMap::new()).unwrap_err();
        assert!(matches!(err, ArbiterError::ReadyTimeout { .. }));
        for j in joins {
            let _ = j.join();
        }
    }
}
