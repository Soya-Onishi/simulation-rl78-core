//! Arbiter: bind control resources, spawn nodes, Ready barrier, Start.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::control::{
    ControlMessage, accept_timeout, bind_listener, default_control_dir, read_message_timeout,
    write_message,
};
use crate::topology::{LogicalTopology, TopologyError};

/// Arbiter failures.
#[derive(Debug, thiserror::Error)]
pub enum ArbiterError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("topology error: {0}")]
    Topology(#[from] TopologyError),
    #[error("ready barrier timed out after {timeout_ms}ms; missing boards: {missing}")]
    ReadyTimeout { timeout_ms: u64, missing: String },
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

    let result = ready_barrier_and_start(topo, &mut listeners);
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

fn ready_barrier_and_start(
    topo: &LogicalTopology,
    listeners: &mut [(String, PathBuf, std::os::unix::net::UnixListener)],
) -> Result<(), ArbiterError> {
    let timeout = Duration::from_millis(topo.ready_timeout_ms);
    let deadline = Instant::now() + timeout;
    let expected: Vec<String> = topo.boards.iter().map(|b| b.id.clone()).collect();
    let mut streams = HashMap::new();

    // Accept one connection per board (nodes connect to their dedicated socket).
    for (board_id, _sock, listener) in listeners.iter() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Some(mut stream) = accept_timeout(listener, remaining)? else {
            let missing = expected
                .iter()
                .filter(|id| !streams.contains_key(*id))
                .cloned()
                .collect::<Vec<_>>()
                .join(",");
            return Err(ArbiterError::ReadyTimeout {
                timeout_ms: topo.ready_timeout_ms,
                missing,
            });
        };

        let startup = ControlMessage::StartupRecord {
            board_id: board_id.clone(),
            shm_segments: Vec::new(),
        };
        write_message(&mut stream, &startup)?;
        streams.insert(board_id.clone(), stream);
    }

    // Collect Ready from every connected node.
    let mut ready = std::collections::HashSet::new();
    while ready.len() < expected.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let missing = expected
                .iter()
                .filter(|id| !ready.contains(*id))
                .cloned()
                .collect::<Vec<_>>()
                .join(",");
            return Err(ArbiterError::ReadyTimeout {
                timeout_ms: topo.ready_timeout_ms,
                missing,
            });
        }

        let mut progress = false;
        for (board_id, stream) in streams.iter_mut() {
            if ready.contains(board_id) {
                continue;
            }
            // Short poll per socket so we can rotate among peers.
            let slice = remaining.min(Duration::from_millis(50));
            match read_message_timeout(stream, slice) {
                Ok(ControlMessage::Ready { board_id: id }) => {
                    if &id != board_id {
                        return Err(ArbiterError::Message(format!(
                            "ready from unexpected board '{id}' on socket for '{board_id}'"
                        )));
                    }
                    ready.insert(id);
                    progress = true;
                }
                Ok(other) => {
                    return Err(ArbiterError::Message(format!(
                        "expected Ready from '{board_id}', got {other:?}"
                    )));
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::TimedOut
                        || err.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    // try next peer
                }
                Err(err) => return Err(ArbiterError::Io(err)),
            }
        }
        if !progress {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    eprintln!(
        "cluster-arbiter: Ready barrier complete ({} nodes)",
        ready.len()
    );

    for stream in streams.values_mut() {
        write_message(stream, &ControlMessage::Start)?;
    }
    eprintln!("cluster-arbiter: Start broadcast");
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
        // Retry connect until the listener exists.
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
            write_message(
                &mut stream,
                &ControlMessage::Ready {
                    board_id: board_id.clone(),
                },
            )
            .unwrap();
            let start = crate::control::read_message(&mut stream).unwrap();
            assert!(matches!(start, ControlMessage::Start));
        } else {
            // Stay quiet until the arbiter drops the socket on timeout.
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
        ready_barrier_and_start(&topo, &mut listeners).unwrap();
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
            let send = i == 0; // only first board sends Ready
            joins.push(thread::spawn(move || fake_node(sock2, id, send)));
            listeners.push((board.id.clone(), sock, listener));
        }
        let err = ready_barrier_and_start(&topo, &mut listeners).unwrap_err();
        assert!(matches!(err, ArbiterError::ReadyTimeout { .. }));
        for j in joins {
            let _ = j.join();
        }
    }
}
