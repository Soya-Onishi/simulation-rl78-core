//! Cluster node: event-driven control handshake + B-lite UART receive.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::blit::{UartInbox, UartRxThread};
use crate::control::{ControlMessage, ShmBinding, ShmRole, read_message, write_message};
use crate::lifecycle::{NodeEffect, NodeState};
use crate::shm_uart::{ShmUartError, UartShmEndpoint};

/// Node-side failures (I/O / unrecoverable only).
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("shm error: {0}")]
    Shm(#[from] ShmUartError),
    #[error("{0}")]
    Message(String),
}

/// Resources attached for one node process.
struct NodeAttachments {
    /// Producer endpoints keyed by edge id (sim thread will push later).
    #[allow(dead_code)]
    producers: HashMap<String, UartShmEndpoint>,
    /// Per-consumer inbox for B-lite drain.
    inboxes: HashMap<String, Arc<UartInbox>>,
    /// Receive threads (dropped on shutdown).
    #[allow(dead_code)]
    rx_threads: Vec<UartRxThread>,
}

impl NodeAttachments {
    fn attach(board_id: &str, segments: &[ShmBinding]) -> Result<Self, NodeError> {
        let mut producers = HashMap::new();
        let mut inboxes = HashMap::new();
        let mut rx_threads = Vec::new();
        for seg in segments {
            match seg.role {
                ShmRole::Producer => {
                    let ep = UartShmEndpoint::open(PathBuf::from(&seg.flink_name))?;
                    eprintln!(
                        "cluster-node[{board_id}]: producer attach {} ({})",
                        seg.edge_id, seg.flink_name
                    );
                    producers.insert(seg.edge_id.clone(), ep);
                }
                ShmRole::Consumer => {
                    let inbox = UartInbox::new(256);
                    let rx = UartRxThread::spawn(
                        PathBuf::from(&seg.flink_name),
                        Arc::clone(&inbox),
                        seg.edge_id.clone(),
                    )?;
                    eprintln!(
                        "cluster-node[{board_id}]: consumer attach {} ({})",
                        seg.edge_id, seg.flink_name
                    );
                    inboxes.insert(seg.edge_id.clone(), inbox);
                    rx_threads.push(rx);
                }
            }
        }
        Ok(Self {
            producers,
            inboxes,
            rx_threads,
        })
    }

    /// Drain all consumer inboxes (quantum-entry hook will call this later).
    #[allow(dead_code)]
    fn drain_all(
        &self,
        limit_per_edge: usize,
    ) -> Vec<(String, Vec<crate::shm_uart::ShmUartFrame>)> {
        self.inboxes
            .iter()
            .map(|(id, inbox)| {
                if inbox.take_overflow_warning() {
                    eprintln!("cluster-node: warning: uart inbox overflow on {id}");
                }
                (id.clone(), inbox.drain(limit_per_edge))
            })
            .collect()
    }
}

/// Connect to the arbiter control socket and drive the node state machine.
///
/// Phase 3: attaches UART SHM and starts B-lite RX threads before Ready.
pub fn run_node(board_id: &str, control: &Path) -> Result<(), NodeError> {
    let mut stream = connect_with_retry(control, Duration::from_secs(5))?;
    let mut state = NodeState::AwaitingStartup;
    let mut attachments: Option<NodeAttachments> = None;

    while !matches!(state, NodeState::Running | NodeState::Stopped) {
        let msg = read_message(&mut stream)?;
        if let ControlMessage::StartupRecord {
            board_id: ref id,
            ref shm_segments,
        } = msg
            && id == board_id
            && attachments.is_none()
        {
            attachments = Some(NodeAttachments::attach(board_id, shm_segments)?);
        }
        let (next, effect) = state.on_message(board_id, msg);
        if let Some(effect) = effect {
            apply_effect(board_id, &mut stream, effect)?;
        }
        if next != state {
            eprintln!("cluster-node[{board_id}]: {state:?} -> {next:?}");
        }
        state = next;
    }

    if state == NodeState::Running {
        // Hold briefly so the arbiter can observe a live child with RX threads up.
        let _ = &attachments;
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn apply_effect(
    board_id: &str,
    stream: &mut UnixStream,
    effect: NodeEffect,
) -> Result<(), NodeError> {
    match effect {
        NodeEffect::SendReady => {
            write_message(
                stream,
                &ControlMessage::Ready {
                    board_id: board_id.to_string(),
                },
            )?;
            eprintln!("cluster-node[{board_id}]: Ready");
        }
        NodeEffect::Warn(msg) => {
            eprintln!("cluster-node[{board_id}]: warning: {msg}");
        }
    }
    Ok(())
}

fn connect_with_retry(path: &Path, budget: Duration) -> Result<UnixStream, NodeError> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if std::time::Instant::now() >= deadline {
                    return Err(NodeError::Io(err));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Helper used by the binary for typed args.
#[derive(Clone, Debug)]
pub struct NodeOptions {
    pub board_id: String,
    pub control: PathBuf,
}
