//! Cluster node: event-driven control handshake + B-lite UART receive.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::blit::{UartInbox, UartRxThread};
use crate::control::{
    ControlMessage, ShmBinding, ShmRole, read_message, read_message_timeout, write_message,
};
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

/// Optional HostStop injection for smoke / E2E without a wired Machine.
#[derive(Clone, Debug)]
pub struct InjectHostStop {
    pub after_ms: u64,
    pub reason: String,
}

/// Connect to the arbiter control socket and drive the node state machine.
pub fn run_node(opts: NodeOptions) -> Result<(), NodeError> {
    let board_id = opts.board_id.as_str();
    let mut stream = connect_with_retry(&opts.control, Duration::from_secs(5))?;
    let mut state = NodeState::AwaitingStartup;
    let mut attachments: Option<NodeAttachments> = None;
    let mut headroom_threshold_ns = 0_u64;
    let mut allowed_ns = 0_u64;
    let mut virtual_time_ns = 0_u64;

    while !matches!(state, NodeState::Running | NodeState::Stopped) {
        let msg = read_message(&mut stream)?;
        if let ControlMessage::StartupRecord {
            board_id: ref id,
            ref shm_segments,
            headroom_threshold_ns: thr,
            ..
        } = msg
            && id == board_id
            && attachments.is_none()
        {
            headroom_threshold_ns = thr;
            attachments = Some(NodeAttachments::attach(board_id, shm_segments)?);
        }
        let (next, effect) = state.on_message(board_id, msg);
        if let Some(effect) = effect {
            apply_effect(board_id, &mut stream, effect, &mut allowed_ns)?;
        }
        if next != state {
            eprintln!("cluster-node[{board_id}]: {state:?} -> {next:?}");
        }
        state = next;
    }

    if state == NodeState::Running {
        let _ = &attachments;
        run_while_running(
            board_id,
            &mut stream,
            &mut state,
            &mut allowed_ns,
            &mut virtual_time_ns,
            headroom_threshold_ns,
            opts.inject_host_stop.as_ref(),
        )?;
    }
    Ok(())
}

/// Notify the arbiter of a host-initiated stop when the reason is cluster-relevant.
///
/// Returns `Ok(true)` if [`ControlMessage::HostStop`] was sent.
pub fn notify_host_stop(
    stream: &mut UnixStream,
    board_id: &str,
    reason: &str,
) -> Result<bool, NodeError> {
    use crate::cluster_stop::is_cluster_relevant_reason;

    if !is_cluster_relevant_reason(reason) {
        return Ok(false);
    }
    write_message(
        stream,
        &ControlMessage::HostStop {
            board_id: board_id.to_string(),
            reason: reason.to_string(),
        },
    )?;
    Ok(true)
}

fn run_while_running(
    board_id: &str,
    stream: &mut UnixStream,
    state: &mut NodeState,
    allowed_ns: &mut u64,
    virtual_time_ns: &mut u64,
    headroom_threshold_ns: u64,
    inject: Option<&InjectHostStop>,
) -> Result<(), NodeError> {
    let started = Instant::now();
    let deadline = started + Duration::from_millis(200);
    let mut host_stop_sent = false;
    while Instant::now() < deadline && *state == NodeState::Running {
        if let Some(inj) = inject
            && !host_stop_sent
            && started.elapsed() >= Duration::from_millis(inj.after_ms)
        {
            match notify_host_stop(stream, board_id, &inj.reason)? {
                true => {
                    eprintln!(
                        "cluster-node[{board_id}]: HostStop injected ({})",
                        inj.reason
                    );
                    *state = NodeState::Stopped;
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

        match read_message_timeout(stream, Duration::from_millis(10)) {
            Ok(msg) => {
                let (next, effect) = state.on_message(board_id, msg);
                if let Some(effect) = effect {
                    apply_effect(board_id, stream, effect, allowed_ns)?;
                }
                *state = next;
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::TimedOut
                    || err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                *state = NodeState::Stopped;
                break;
            }
            Err(err) => return Err(NodeError::Io(err)),
        }

        // TODO(phase6): Remove this placeholder clock once Machine is wired; virtual
        // time must come from sim-kernel's clock, not a local creep.
        // Placeholder so Allowed/TimeReport can be exercised without Machine.
        if *virtual_time_ns < *allowed_ns {
            *virtual_time_ns = (*virtual_time_ns + 1).min(*allowed_ns);
        }
        let headroom = allowed_ns.saturating_sub(*virtual_time_ns);
        if headroom < headroom_threshold_ns {
            match write_message(
                stream,
                &ControlMessage::TimeReport {
                    board_id: board_id.to_string(),
                    virtual_time_ns: *virtual_time_ns,
                },
            ) {
                Ok(()) => {}
                Err(err)
                    if err.kind() == std::io::ErrorKind::BrokenPipe
                        || err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    *state = NodeState::Stopped;
                    break;
                }
                Err(err) => return Err(NodeError::Io(err)),
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn apply_effect(
    board_id: &str,
    stream: &mut UnixStream,
    effect: NodeEffect,
    allowed_ns: &mut u64,
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

fn connect_with_retry(path: &Path, budget: Duration) -> Result<UnixStream, NodeError> {
    let deadline = Instant::now() + budget;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if Instant::now() >= deadline {
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
    /// When set, send HostStop after Start (MVP smoke without Machine).
    pub inject_host_stop: Option<InjectHostStop>,
}
