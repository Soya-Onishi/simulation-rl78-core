//! Simulation control backend.
//!
//! [`SimulationBackend`] is the process/module boundary: today
//! [`LocalBackend`] talks to the in-process worker; later an HTTP client
//! implementation can target a remote sim-server with the same contract.

use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use crate::contract::{ActionList, ApiError, InvokeActionRequest, LogEvent, StatusEvent};
use crate::host::HostCommand;
use crate::server::{self, Job, JobKind, JobReply};
use crate::topology::LogicalTopology;

/// Control-plane operations shared by HTTP adapters.
pub trait SimulationBackend: Send + Sync {
    fn create(&self, topology: LogicalTopology) -> Result<String, ApiError>;
    fn start(&self, id: &str, until_ns: Option<u64>) -> Result<(), ApiError>;
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
    fn actions(&self, id: &str) -> Result<ActionList, ApiError>;
    fn invoke_action(&self, id: &str, request: InvokeActionRequest) -> Result<(), ApiError>;
    fn probe_catalog(&self, id: &str) -> Result<Vec<crate::probe_log::ProbeCatalogItem>, ApiError>;
    fn probe_watch_get(&self, id: &str) -> Result<crate::probe_log::ProbeWatchView, ApiError>;
    fn probe_watch_put(
        &self,
        id: &str,
        probes: Vec<crate::probe_log::ProbeRef>,
    ) -> Result<crate::probe_log::ProbeWatchView, ApiError>;
    fn probe_watch_clear(&self, id: &str) -> Result<(), ApiError>;
    fn probe_watch_live(
        &self,
        id: &str,
        after: u64,
    ) -> Result<Vec<crate::probe_log::CoarseBatch>, ApiError>;
    fn csv_start(
        &self,
        id: &str,
        range: crate::probe_log::ProbeRange,
    ) -> Result<(u64, Vec<crate::probe_log::ProbePointName>), ApiError>;
    fn csv_poll(&self, id: &str, request_id: u64) -> Result<crate::probe_log::CsvPoll, ApiError>;
    fn csv_status(&self, id: &str, request_id: u64) -> Result<crate::probe_log::CsvPoll, ApiError>;
    fn probe_samples(
        &self,
        id: &str,
        board: &str,
        probe: &str,
        from_ns: Option<u64>,
        to_ns: Option<u64>,
    ) -> Result<crate::contract::ProbeSamplesResponse, ApiError>;
    fn viewer_attach(&self, id: &str) -> Result<String, ApiError>;
    fn viewer_check(&self, id: &str, token: &str) -> Result<(), ApiError>;
    fn viewer_detach(&self, id: &str, token: &str) -> Result<(), ApiError>;
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
        self.rpc_timeout(kind, Duration::from_secs(60))
    }

    fn rpc_timeout(&self, kind: JobKind, timeout: Duration) -> Result<JobReply, ApiError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Job {
                kind,
                reply: reply_tx,
            })
            .map_err(|_| ApiError::internal("cluster-server worker is not running"))?;
        reply_rx
            .recv_timeout(timeout)
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

    fn start(&self, id: &str, until_ns: Option<u64>) -> Result<(), ApiError> {
        self.command(id, HostCommand::Start { until_ns })
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

    fn actions(&self, id: &str) -> Result<ActionList, ApiError> {
        match self.rpc(JobKind::ListActions { id: id.to_string() })? {
            JobReply::Actions(list) => Ok(list),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for actions: {other:?}"
            ))),
        }
    }

    fn invoke_action(&self, id: &str, request: InvokeActionRequest) -> Result<(), ApiError> {
        match self.rpc(JobKind::Invoke {
            id: id.to_string(),
            request,
        })? {
            JobReply::Accepted => Ok(()),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for invoke: {other:?}"
            ))),
        }
    }

    fn probe_catalog(&self, id: &str) -> Result<Vec<crate::probe_log::ProbeCatalogItem>, ApiError> {
        match self.rpc(JobKind::ProbeCatalog { id: id.to_string() })? {
            JobReply::ProbeCatalog(items) => Ok(items),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for probe catalog: {other:?}"
            ))),
        }
    }

    fn probe_watch_get(&self, id: &str) -> Result<crate::probe_log::ProbeWatchView, ApiError> {
        match self.rpc(JobKind::ProbeWatchGet { id: id.to_string() })? {
            JobReply::ProbeWatch(view) => Ok(view),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for watch get: {other:?}"
            ))),
        }
    }

    fn probe_watch_put(
        &self,
        id: &str,
        probes: Vec<crate::probe_log::ProbeRef>,
    ) -> Result<crate::probe_log::ProbeWatchView, ApiError> {
        match self.rpc(JobKind::ProbeWatchPut {
            id: id.to_string(),
            probes,
        })? {
            JobReply::ProbeWatch(view) => Ok(view),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for watch put: {other:?}"
            ))),
        }
    }

    fn probe_watch_clear(&self, id: &str) -> Result<(), ApiError> {
        match self.rpc(JobKind::ProbeWatchClear { id: id.to_string() })? {
            JobReply::Accepted => Ok(()),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for watch clear: {other:?}"
            ))),
        }
    }

    fn probe_watch_live(
        &self,
        id: &str,
        after: u64,
    ) -> Result<Vec<crate::probe_log::CoarseBatch>, ApiError> {
        match self.rpc(JobKind::ProbeWatchLive {
            id: id.to_string(),
            after,
        })? {
            JobReply::ProbeCoarse(batches) => Ok(batches),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for watch live: {other:?}"
            ))),
        }
    }

    fn csv_start(
        &self,
        id: &str,
        range: crate::probe_log::ProbeRange,
    ) -> Result<(u64, Vec<crate::probe_log::ProbePointName>), ApiError> {
        match self.rpc(JobKind::ProbeCsvStart {
            id: id.to_string(),
            range,
        })? {
            JobReply::ProbeCsvStart {
                request_id,
                catalog,
            } => Ok((request_id, catalog)),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for csv start: {other:?}"
            ))),
        }
    }

    fn csv_poll(&self, id: &str, request_id: u64) -> Result<crate::probe_log::CsvPoll, ApiError> {
        match self.rpc(JobKind::ProbeCsvPoll {
            id: id.to_string(),
            request_id,
        })? {
            JobReply::ProbeCsvPoll(poll) => Ok(poll),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for csv poll: {other:?}"
            ))),
        }
    }

    fn csv_status(&self, id: &str, request_id: u64) -> Result<crate::probe_log::CsvPoll, ApiError> {
        match self.rpc(JobKind::ProbeCsvStatus {
            id: id.to_string(),
            request_id,
        })? {
            JobReply::ProbeCsvPoll(poll) => Ok(poll),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for csv status: {other:?}"
            ))),
        }
    }

    fn probe_samples(
        &self,
        id: &str,
        board: &str,
        probe: &str,
        from_ns: Option<u64>,
        to_ns: Option<u64>,
    ) -> Result<crate::contract::ProbeSamplesResponse, ApiError> {
        match self.rpc(JobKind::ProbeSamples {
            id: id.to_string(),
            board: board.to_string(),
            probe: probe.to_string(),
            from_ns,
            to_ns,
        })? {
            JobReply::ProbeSamples(body) => Ok(body),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for probe samples: {other:?}"
            ))),
        }
    }

    fn viewer_attach(&self, id: &str) -> Result<String, ApiError> {
        match self.rpc(JobKind::ViewerAttach { id: id.to_string() })? {
            JobReply::ViewerToken(token) => Ok(token),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for viewer attach: {other:?}"
            ))),
        }
    }

    fn viewer_check(&self, id: &str, token: &str) -> Result<(), ApiError> {
        match self.rpc(JobKind::ViewerCheck {
            id: id.to_string(),
            token: token.to_string(),
        })? {
            JobReply::Accepted => Ok(()),
            JobReply::Err(err) => Err(err),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for viewer check: {other:?}"
            ))),
        }
    }

    fn viewer_detach(&self, id: &str, token: &str) -> Result<(), ApiError> {
        match self.rpc(JobKind::ViewerDetach {
            id: id.to_string(),
            token: token.to_string(),
        })? {
            JobReply::Accepted | JobReply::Err(_) => Ok(()),
            other => Err(ApiError::internal(format!(
                "unexpected worker reply for viewer detach: {other:?}"
            ))),
        }
    }
}

/// Validate topology for HTTP create. Local ELF paths are rejected.
pub fn prepare_topology(topology: LogicalTopology) -> Result<LogicalTopology, ApiError> {
    server::prepare_topology(topology)
}
