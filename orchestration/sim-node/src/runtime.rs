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
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};

use contract::{HostCtrl, IpcError, NodeCommand, NodeCommandResult, NodeConfig, NodeLogRecord};

use crate::ipc::ipc_loop;
use crate::logger::{self, DummyTcpStream, log_loop};
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

/// A command forwarded by the IPC thread with its one-shot reply channel.
pub(crate) struct Routed {
    pub(crate) command: NodeCommand,
    pub(crate) reply: SyncSender<NodeCommandResult>,
}

/// Running IPC / Probe / Simulation / log threads. Dropping shuts them down.
pub struct NodeRuntime {
    shutdown: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    threads: Vec<JoinHandle<Result<(), NodeRuntimeError>>>,
}

impl NodeRuntime {
    /// Open `host_ctrl`, start the four threads, and return once the RR
    /// server exists.
    pub fn spawn(config: NodeConfig) -> Result<Self, NodeRuntimeError> {
        let host_ctrl = HostCtrl::open(&config.iox_root, config.board_count)
            .map_err(NodeRuntimeError::HostCtrl)?;
        logger::install();
        let mut runtime = Self {
            shutdown: Arc::new(AtomicBool::new(false)),
            state: Arc::new(AtomicU8::new(SimState::Stopped as u8)),
            threads: Vec::with_capacity(4),
        };
        // Declared after `runtime`: an early return drops `log_tx` first, so
        // the log thread can end while `runtime`'s drop joins it.
        let (log_tx, log_rx) = logger::channel();
        let (sim_tx, sim_rx) = mpsc::channel();
        let (probe_tx, probe_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        runtime.spawn_thread("sim-node-log", move || {
            log_loop(&log_rx, DummyTcpStream);
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
        let shutdown = runtime.shutdown.clone();
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
        let on_exit = ShutdownOnExit(self.shutdown.clone());
        let handle = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _on_exit = on_exit;
                body()
            })
            .map_err(|e| NodeRuntimeError::Thread(format!("spawn {name}: {e}")))?;
        self.threads.push(handle);
        Ok(())
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

/// Raises the shutdown flag when a thread exits, including by panic.
struct ShutdownOnExit(Arc<AtomicBool>);

impl Drop for ShutdownOnExit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn empty_runtime() -> NodeRuntime {
        NodeRuntime {
            shutdown: Arc::new(AtomicBool::new(false)),
            state: Arc::new(AtomicU8::new(SimState::Stopped as u8)),
            threads: Vec::new(),
        }
    }

    fn wait_until_stopped(runtime: &NodeRuntime) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while runtime.is_running() {
            assert!(Instant::now() < deadline, "runtime kept running");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn panicking_thread_stops_runtime_and_reports_panic() {
        let mut runtime = empty_runtime();
        let flag = runtime.shutdown.clone();
        runtime
            .spawn_thread("waits-for-shutdown", move || {
                while !flag.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            })
            .unwrap();
        runtime
            .spawn_thread("panics", || panic!("simulated thread failure"))
            .unwrap();

        wait_until_stopped(&runtime);
        assert!(matches!(
            runtime.shutdown(),
            Err(NodeRuntimeError::Thread(message)) if message == "thread panicked"
        ));
    }

    #[test]
    fn returning_thread_stops_runtime() {
        let mut runtime = empty_runtime();
        runtime.spawn_thread("returns", || Ok(())).unwrap();
        wait_until_stopped(&runtime);
        assert!(runtime.shutdown().is_ok());
    }
}
