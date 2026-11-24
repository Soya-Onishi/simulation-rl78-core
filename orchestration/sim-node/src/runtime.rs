//! Node process runtime: IPC / Probe / Simulation threads (#147 / #153).
//!
//! The IPC thread owns the Command ↔ Node RR server and forwards each request
//! over an mpsc channel with a one-shot reply channel: `ProbeSubscribe` to the
//! Probe thread, everything else to the Simulation thread.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use contract::{
    ClusterId, HashedId, HostCtrl, IpcError, NodeCommand, NodeCommandError, NodeCommandResult,
    create_iox_node, create_node_command_server, isolated_config,
};

use crate::probe::ProbeSubscriptions;
use crate::simulation::{SimState, Simulation};

/// IPC sleep when the RR server has no pending request.
const IPC_IDLE_SLEEP: Duration = Duration::from_millis(1);
/// Probe thread period.
const PROBE_PERIOD: Duration = Duration::from_millis(100);
/// Simulation thread `stop_generation` poll period.
const STOP_POLL_PERIOD: Duration = Duration::from_millis(1);

/// Identity and shared-memory location of one Node process.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// iceoryx `root_path` and `host_ctrl` directory for the cluster on this host.
    pub iox_root: PathBuf,
    pub cluster: ClusterId,
    pub node_key: HashedId<u64>,
    /// Board count of this host's `host_ctrl` mapping.
    pub board_count: u32,
}

#[derive(Debug)]
pub enum NodeRuntimeError {
    HostCtrl(io::Error),
    Ipc(IpcError),
    Thread(String),
}

impl fmt::Display for NodeRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostCtrl(e) => write!(f, "host_ctrl: {e}"),
            Self::Ipc(e) => write!(f, "ipc: {e}"),
            Self::Thread(e) => write!(f, "thread: {e}"),
        }
    }
}

impl std::error::Error for NodeRuntimeError {}

struct Routed {
    command: NodeCommand,
    reply: SyncSender<NodeCommandResult>,
}

/// Running IPC / Probe / Simulation threads. Dropping shuts them down.
pub struct NodeRuntime {
    shutdown: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    threads: Vec<JoinHandle<Result<(), NodeRuntimeError>>>,
}

impl NodeRuntime {
    /// Open `host_ctrl`, start the three threads, and return once the RR
    /// server exists.
    pub fn spawn(config: NodeConfig) -> Result<Self, NodeRuntimeError> {
        let host_ctrl = HostCtrl::open(&config.iox_root, config.board_count)
            .map_err(NodeRuntimeError::HostCtrl)?;
        let mut runtime = Self {
            shutdown: Arc::new(AtomicBool::new(false)),
            state: Arc::new(AtomicU8::new(SimState::Stopped as u8)),
            threads: Vec::with_capacity(3),
        };
        let (sim_tx, sim_rx) = mpsc::channel();
        let (probe_tx, probe_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let shutdown = runtime.shutdown.clone();
        let state = runtime.state.clone();
        runtime.spawn_thread("sim-node-simulation", move || {
            simulation_loop(&host_ctrl, &sim_rx, &state, &shutdown);
            Ok(())
        })?;
        let shutdown = runtime.shutdown.clone();
        runtime.spawn_thread("sim-node-probe", move || {
            probe_loop(&probe_rx, &shutdown);
            Ok(())
        })?;
        let shutdown = runtime.shutdown.clone();
        runtime.spawn_thread("sim-node-ipc", move || {
            ipc_loop(&config, &sim_tx, &probe_tx, &shutdown, &ready_tx)
        })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(runtime),
            Ok(Err(e)) => Err(NodeRuntimeError::Ipc(e)),
            Err(_) => Err(NodeRuntimeError::Thread(
                "ipc thread exited before ready".into(),
            )),
        }
    }

    /// Current Simulation run state.
    #[must_use]
    pub fn state(&self) -> SimState {
        SimState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// `false` once any thread has exited (shutdown or a fatal IPC error).
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.shutdown.load(Ordering::Acquire)
    }

    /// Stop every thread and return the first thread error.
    pub fn shutdown(mut self) -> Result<(), NodeRuntimeError> {
        self.shutdown.store(true, Ordering::Release);
        self.join_threads()
    }

    fn spawn_thread(
        &mut self,
        name: &str,
        body: impl FnOnce() -> Result<(), NodeRuntimeError> + Send + 'static,
    ) -> Result<(), NodeRuntimeError> {
        let shutdown = self.shutdown.clone();
        let handle = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let result = body();
                shutdown.store(true, Ordering::Release);
                result
            })
            .map_err(|e| NodeRuntimeError::Thread(format!("spawn {name}: {e}")))?;
        self.threads.push(handle);
        Ok(())
    }

    fn join_threads(&mut self) -> Result<(), NodeRuntimeError> {
        let mut first = Ok(());
        for handle in self.threads.drain(..) {
            let result = handle
                .join()
                .unwrap_or_else(|_| Err(NodeRuntimeError::Thread("thread panicked".into())));
            if first.is_ok() {
                first = result;
            }
        }
        first
    }
}

impl Drop for NodeRuntime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.join_threads();
    }
}

fn ipc_loop(
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
            thread::sleep(IPC_IDLE_SLEEP);
            continue;
        };
        let result = route(*request.payload(), sim_tx, probe_tx);
        if let Err(e) = request.send_copy(result) {
            log::warn!("node command response dropped: {e:?}");
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

fn simulation_loop(
    host_ctrl: &HostCtrl,
    rx: &Receiver<Routed>,
    state: &AtomicU8,
    shutdown: &AtomicBool,
) {
    let mut sim = Simulation::new(host_ctrl.stop_generation());
    while !shutdown.load(Ordering::Acquire) {
        match rx.recv_timeout(STOP_POLL_PERIOD) {
            Ok(Routed { command, reply }) => {
                let result = sim.apply(command);
                state.store(sim.state() as u8, Ordering::Release);
                let _ = reply.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if sim.observe_stop_generation(host_ctrl.stop_generation()) {
            state.store(sim.state() as u8, Ordering::Release);
            log::info!("emergency stop: host_ctrl stop_generation advanced");
        }
    }
}

fn probe_loop(rx: &Receiver<Routed>, shutdown: &AtomicBool) {
    let mut subscriptions = ProbeSubscriptions::default();
    while !shutdown.load(Ordering::Acquire) {
        match rx.recv_timeout(PROBE_PERIOD) {
            Ok(Routed { command, reply }) => {
                let result = match command {
                    NodeCommand::ProbeSubscribe { count, probe_ids } => {
                        subscriptions.subscribe(&probe_ids[..usize::from(count)])
                    }
                    _ => NodeCommandResult::Error(NodeCommandError::ImplFault),
                };
                let _ = reply.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}
