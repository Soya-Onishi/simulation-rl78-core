//! Simulation control backend.
//!
//! [`SimulationBackend`] is the process/module boundary: today
//! [`LocalBackend`] talks to the in-process worker; later an HTTP client
//! implementation can target a remote sim-server with the same contract.

use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use crate::contract::{ApiError, LogEvent, StatusEvent};
use crate::host::HostCommand;
use crate::server::{self, Job, JobKind, JobReply};
use crate::topology::LogicalTopology;

/// Control-plane operations shared by HTTP adapters.
pub trait SimulationBackend: Send + Sync {
    fn create(&self, topology: LogicalTopology) -> Result<String, ApiError>;
    fn start(&self, id: &str) -> Result<(), ApiError>;
    fn stop(&self, id: &str) -> Result<(), ApiError>;
    fn reset(&self, id: &str) -> Result<(), ApiError>;
    fn delete(&self, id: &str) -> Result<(), ApiError>;
    fn exists(&self, id: &str) -> Result<bool, ApiError>;
    fn status(&self, id: &str) -> Result<StatusEvent, ApiError>;
    /// Logs with `seq > after`. Use the high-water mark at subscribe time for
    /// "from connection onward" streaming.
    fn logs_after(&self, id: &str, after: u64) -> Result<Vec<LogEvent>, ApiError>;
    /// Highest log seq currently stored (`0` if none).
    fn log_high_water(&self, id: &str) -> Result<u64, ApiError>;
}

/// In-process backend over the cluster-server worker queue.
#[derive(Clone)]
pub struct LocalBackend {
    tx: Sender<Job>,
}

impl LocalBackend {
    #[must_use]
    pub fn new(tx: Sender<Job>) -> Arc<Self> {
        Arc::new(Self { tx })
    }

    fn rpc(&self, kind: JobKind) -> Result<JobReply, ApiError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Job {
                kind,
                reply: reply_tx,
            })
            .map_err(|_| ApiError::internal("cluster-server worker is not running"))?;
        reply_rx
            .recv_timeout(Duration::from_secs(60))
            .map_err(|_| ApiError::internal("cluster-server worker timed out"))
    }

    fn command(&self, id: &str, cmd: HostCommand) -> Result<(), ApiError> {
        match self.rpc(JobKind::Command {
            id: id.to_string(),
            cmd,
        })? {
            JobReply::Accepted => Ok(()),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for command: {other:?}"
            ))),
        }
    }
}

impl SimulationBackend for LocalBackend {
    fn create(&self, topology: LogicalTopology) -> Result<String, ApiError> {
        match self.rpc(JobKind::Create(topology))? {
            JobReply::Created { id } => Ok(id),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for create: {other:?}"
            ))),
        }
    }

    fn start(&self, id: &str) -> Result<(), ApiError> {
        self.command(id, HostCommand::Start)
    }

    fn stop(&self, id: &str) -> Result<(), ApiError> {
        self.command(id, HostCommand::Stop)
    }

    fn reset(&self, id: &str) -> Result<(), ApiError> {
        self.command(id, HostCommand::Reset)
    }

    fn delete(&self, id: &str) -> Result<(), ApiError> {
        match self.rpc(JobKind::Delete { id: id.to_string() })? {
            JobReply::Deleted => Ok(()),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for delete: {other:?}"
            ))),
        }
    }

    fn exists(&self, id: &str) -> Result<bool, ApiError> {
        match self.rpc(JobKind::Exists { id: id.to_string() })? {
            JobReply::Exists(flag) => Ok(flag),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for exists: {other:?}"
            ))),
        }
    }

    fn status(&self, id: &str) -> Result<StatusEvent, ApiError> {
        match self.rpc(JobKind::GetStatus { id: id.to_string() })? {
            JobReply::Status(status) => Ok(status),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for status: {other:?}"
            ))),
        }
    }

    fn logs_after(&self, id: &str, after: u64) -> Result<Vec<LogEvent>, ApiError> {
        match self.rpc(JobKind::GetLogs {
            id: id.to_string(),
            after,
        })? {
            JobReply::Logs(logs) => Ok(logs),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for logs: {other:?}"
            ))),
        }
    }

    fn log_high_water(&self, id: &str) -> Result<u64, ApiError> {
        match self.rpc(JobKind::LogHighWater { id: id.to_string() })? {
            JobReply::HighWater(seq) => Ok(seq),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for log high water: {other:?}"
            ))),
        }
    }
}

/// Validate topology and embed local ELF paths before enqueueing create.
pub fn prepare_topology(
    topology: LogicalTopology,
    base_dir: &std::path::Path,
) -> Result<LogicalTopology, ApiError> {
    server::prepare_topology(topology, base_dir)
}
