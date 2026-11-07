//! Simulation control backend.
//!
//! [`SimulationBackend`] is the process/module boundary.
//! [`LocalBackend`] talks to the in-process worker.
//! [`TcpipBackend`] talks to a remote `cluster-backend` over HTTP.

mod local;
mod serve;
mod tcpip;
mod wire;

pub use local::LocalBackend;
pub use serve::serve_internal;
pub use tcpip::{TcpipBackend, check_health};

use crate::contract::{
    ActionList, ApiError, InvokeActionRequest, LogEvent, StatusEvent, TopologyView,
};
use crate::inspect::{InspectReply, InspectRequest};
use crate::topology::LogicalTopology;

/// Control-plane operations shared by HTTP adapters.
pub trait SimulationBackend: Send + Sync {
    fn create(&self, topology: LogicalTopology) -> Result<String, ApiError>;
    fn start(&self, id: &str, until_ns: Option<u64>) -> Result<(), ApiError>;
    fn stop(&self, id: &str) -> Result<(), ApiError>;
    fn reset(&self, id: &str) -> Result<(), ApiError>;
    fn shutdown(&self, id: &str) -> Result<(), ApiError>;
    fn exists(&self, id: &str) -> Result<bool, ApiError>;
    fn status(&self, id: &str) -> Result<StatusEvent, ApiError>;
    fn topology(&self, id: &str) -> Result<TopologyView, ApiError>;
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
        board: crate::ipc::HashedId<u64>,
        probe: &str,
        from_ns: Option<u64>,
        to_ns: Option<u64>,
    ) -> Result<crate::contract::ProbeSamplesResponse, ApiError>;
    fn viewer_attach(&self, id: &str) -> Result<String, ApiError>;
    fn viewer_check(&self, id: &str, token: &str) -> Result<(), ApiError>;
    fn viewer_detach(&self, id: &str, token: &str) -> Result<(), ApiError>;
    fn inspect(
        &self,
        id: &str,
        board_id: crate::ipc::HashedId<u64>,
        req: InspectRequest,
    ) -> Result<InspectReply, ApiError>;
    fn step(&self, id: &str, board_id: crate::ipc::HashedId<u64>) -> Result<(), ApiError>;
}
