//! Command host: configuration, stale-session cleanup, and the four threads.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use axum::Router;
use contract::{
    ClusterId, IpcError, ThreadGroup, ThreadPanicked, isolated_config, remove_dead_nodes,
};

use crate::build::{BuildClient, build_loop};
use crate::command::{CommandRequest, command_loop};
use crate::endpoint::endpoint_loop;
use crate::probe::probe_loop;
use crate::router::{CommandHandle, command_router};

/// Command host settings.
#[derive(Clone, Debug)]
pub struct CommandConfig {
    /// Owned by this Command host: each session's `iox_root` is a
    /// subdirectory, and leftovers from earlier runs are removed on spawn.
    pub work_dir: PathBuf,
    /// Node executable the Build thread launches with `NodeConfig::to_args`.
    pub node_program: PathBuf,
    /// Deadline of one Command → Node RR request, including waiting for a
    /// starting Node to create its server.
    pub node_timeout: Duration,
    /// How long terminate / restart wait for a Node to exit before killing it.
    pub terminate_timeout: Duration,
}

impl CommandConfig {
    #[must_use]
    pub fn new(work_dir: PathBuf, node_program: PathBuf) -> Self {
        Self {
            work_dir,
            node_program,
            node_timeout: Duration::from_secs(5),
            terminate_timeout: Duration::from_secs(5),
        }
    }

    /// `iox_root` (and `host_ctrl` directory) of session `cluster`.
    #[must_use]
    pub fn session_root(&self, cluster: ClusterId) -> PathBuf {
        self.work_dir.join(cluster.to_string())
    }
}

#[derive(Debug)]
pub enum CommandHostError {
    /// Work directory or session directory I/O.
    Io(io::Error),
    Ipc(IpcError),
    Thread(String),
}

impl fmt::Display for CommandHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Ipc(e) => write!(f, "ipc: {e}"),
            Self::Thread(e) => write!(f, "thread: {e}"),
        }
    }
}

impl std::error::Error for CommandHostError {}

impl From<ThreadPanicked> for CommandHostError {
    fn from(value: ThreadPanicked) -> Self {
        Self::Thread(value.to_string())
    }
}

/// Running Command / Build / Probe / Endpoint threads. Dropping shuts them
/// down; the Command thread first terminates every Node it launched.
pub struct CommandHost {
    requests: mpsc::Sender<CommandRequest>,
    threads: ThreadGroup<CommandHostError>,
}

impl CommandHost {
    /// Remove sessions left in `work_dir` by an earlier run, then start the threads.
    pub fn spawn(config: CommandConfig) -> Result<Self, CommandHostError> {
        remove_stale_sessions(&config.work_dir)?;
        let config = Arc::new(config);
        let mut threads = ThreadGroup::new();
        // Declared after `threads`: an early return drops the senders first,
        // so the threads already spawned can end while `threads` joins them.
        let (requests_tx, requests_rx) = mpsc::channel();
        let (build_tx, build_rx) = mpsc::channel();
        let (endpoint_tx, endpoint_rx) = mpsc::channel();
        let spawn_error =
            |name: &str, e: io::Error| CommandHostError::Thread(format!("spawn {name}: {e}"));

        let build_config = config.clone();
        threads
            .spawn("sim-command-build", move || {
                build_loop(&build_config, &build_rx);
                Ok(())
            })
            .map_err(|e| spawn_error("build", e))?;
        threads
            .spawn("sim-command-endpoint", move || {
                endpoint_loop(&endpoint_rx);
                Ok(())
            })
            .map_err(|e| spawn_error("endpoint", e))?;
        let shutdown = threads.shutdown_flag();
        threads
            .spawn("sim-command-probe", move || {
                probe_loop(&shutdown);
                Ok(())
            })
            .map_err(|e| spawn_error("probe", e))?;
        let shutdown = threads.shutdown_flag();
        threads
            .spawn("sim-command-command", move || {
                command_loop(
                    &config,
                    &requests_rx,
                    &BuildClient::new(build_tx),
                    &endpoint_tx,
                    &shutdown,
                )
            })
            .map_err(|e| spawn_error("command", e))?;

        Ok(Self {
            requests: requests_tx,
            threads,
        })
    }

    /// Orchestrator → Command REST API backed by this host's Command thread.
    pub fn router(&self) -> Router {
        command_router(CommandHandle::new(self.requests.clone()))
    }

    /// `false` once shutdown was requested or any thread has exited.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.threads.is_running()
    }

    /// Terminate every Node, stop the threads, and return the first error.
    pub fn shutdown(mut self) -> Result<(), CommandHostError> {
        self.threads.shutdown()
    }
}

/// Only directories named like [`CommandConfig::session_root`] are touched.
fn remove_stale_sessions(work_dir: &Path) -> Result<(), CommandHostError> {
    std::fs::create_dir_all(work_dir).map_err(CommandHostError::Io)?;
    for entry in std::fs::read_dir(work_dir).map_err(CommandHostError::Io)? {
        let entry = entry.map_err(CommandHostError::Io)?;
        let is_session = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.parse::<ClusterId>().is_ok());
        if is_session && entry.file_type().map_err(CommandHostError::Io)?.is_dir() {
            remove_session_dir(&entry.path())?;
        }
    }
    Ok(())
}

/// Clean up dead iceoryx nodes under `iox_root`, then delete it unless a live
/// node (e.g. another Command host's) still uses it.
pub(crate) fn remove_session_dir(iox_root: &Path) -> Result<(), CommandHostError> {
    let config = isolated_config(iox_root).map_err(CommandHostError::Ipc)?;
    if remove_dead_nodes(&config).map_err(CommandHostError::Ipc)? == 0 {
        std::fs::remove_dir_all(iox_root).map_err(CommandHostError::Io)?;
    }
    Ok(())
}
