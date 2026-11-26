//! Node process runtime: IPC / Probe / Simulation / log threads (#147 / #153).
//!
//! The IPC thread owns the Command ↔ Node RR server and forwards each request
//! over an mpsc channel with a one-shot reply channel: `ProbeSubscribe` to the
//! Probe thread, everything else to the Simulation thread. Those three threads
//! send their `log` records to the log thread (see [`crate::logger`]).
//!
//! Shutdown: only the IPC thread watches the shutdown flag, and it also exits
//! after answering [`NodeCommand::Shutdown`]. When it exits it drops both
//! senders, so the Simulation and Probe threads end on disconnect after any
//! in-flight request has been answered. The log thread ends once all three
//! have exited and dropped their log senders.

use std::fmt;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, SyncSender};

use contract::{
    HostCtrl, IpcError, NodeCommand, NodeCommandResult, NodeConfig, NodeLogRecord, ThreadGroup,
    ThreadPanicked,
};

use crate::ipc::ipc_loop;
use crate::logger::{self, log_loop};
use crate::probe::probe_loop;
use crate::simulation::{SimState, simulation_loop};

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

impl From<ThreadPanicked> for NodeRuntimeError {
    fn from(value: ThreadPanicked) -> Self {
        Self::Thread(value.to_string())
    }
}

/// A command forwarded by the IPC thread with its one-shot reply channel.
pub(crate) struct Routed {
    pub(crate) command: NodeCommand,
    pub(crate) reply: SyncSender<NodeCommandResult>,
}

/// Running IPC / Probe / Simulation / log threads. Dropping shuts them down.
pub struct NodeRuntime {
    state: Arc<AtomicU8>,
    threads: ThreadGroup<NodeRuntimeError>,
}

impl NodeRuntime {
    /// Open `host_ctrl`, start the four threads, and return once the RR
    /// server exists. The log thread writes JSON lines to `log_stream`.
    pub fn spawn(
        config: NodeConfig,
        log_stream: impl Write + Send + 'static,
    ) -> Result<Self, NodeRuntimeError> {
        let host_ctrl = HostCtrl::open(&config.iox_root, config.board_count)
            .map_err(NodeRuntimeError::HostCtrl)?;
        let mut runtime = Self {
            state: Arc::new(AtomicU8::new(SimState::Stopped as u8)),
            threads: ThreadGroup::new(),
        };
        // Declared after `runtime`: an early return drops `log_tx` first, so
        // the log thread can end while `runtime`'s drop joins it.
        let (log_tx, log_rx) = logger::channel();
        let (sim_tx, sim_rx) = mpsc::channel();
        let (probe_tx, probe_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        runtime.spawn_thread("sim-node-log", move || {
            log_loop(&log_rx, log_stream);
            Ok(())
        })?;
        let state = runtime.state.clone();
        runtime.spawn_logged_thread("sim-node-simulation", &log_tx, move || {
            simulation_loop(&host_ctrl, &sim_rx, &state);
            Ok(())
        })?;
        runtime.spawn_logged_thread("sim-node-probe", &log_tx, move || {
            probe_loop(&probe_rx);
            Ok(())
        })?;
        let shutdown = runtime.threads.shutdown_flag();
        runtime.spawn_logged_thread("sim-node-ipc", &log_tx, move || {
            ipc_loop(&config, &sim_tx, &probe_tx, &shutdown, &ready_tx)
        })?;
        drop(log_tx);

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
        self.threads.is_running()
    }

    /// Stop every thread and return the first thread error.
    pub fn shutdown(mut self) -> Result<(), NodeRuntimeError> {
        self.threads.shutdown()
    }

    fn spawn_thread(
        &mut self,
        name: &str,
        body: impl FnOnce() -> Result<(), NodeRuntimeError> + Send + 'static,
    ) -> Result<(), NodeRuntimeError> {
        self.threads
            .spawn(name, body)
            .map_err(|e| NodeRuntimeError::Thread(format!("spawn {name}: {e}")))
    }

    /// [`Self::spawn_thread`] whose `log` records go to the log thread.
    fn spawn_logged_thread(
        &mut self,
        name: &str,
        log_tx: &SyncSender<NodeLogRecord>,
        body: impl FnOnce() -> Result<(), NodeRuntimeError> + Send + 'static,
    ) -> Result<(), NodeRuntimeError> {
        let log_tx = log_tx.clone();
        self.spawn_thread(name, move || {
            logger::attach(log_tx);
            body()
        })
    }
}
