//! Simulation control backend.
//!
//! [`SimulationBackend`] is the process/module boundary.
//! [`LocalBackend`] talks to the in-process worker.
//! [`TcpipBackend`] talks to a remote `cluster-backend` over HTTP.

mod local;
pub(crate) mod serve;
mod tcpip;
mod wire;

pub use local::LocalBackend;
pub use serve::serve_internal;
pub use tcpip::{TcpipBackend, check_health, fetch_health};
pub use wire::{BackendInfo, HealthBody};

use crate::contract::{
    ActionList, ApiError, InvokeActionRequest, LogEvent, StatusEvent, TopologyView,
};
use crate::inspect::{InspectReply, InspectRequest};
use crate::orchestrate::{
    Phase1CreateRequest, Phase1CreateResponse, Phase2ConnectRequest, Phase2ConnectResponse,
};
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
        board: &str,
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
        board_id: &str,
        req: InspectRequest,
    ) -> Result<InspectReply, ApiError>;
    fn step(&self, id: &str, board_id: &str) -> Result<(), ApiError>;

    /// Multi-backend create phase 1: spawn the local arbiter for this backend's
    /// boards and return its advertised endpoints. Backends that only implement
    /// the single-process path reject this.
    fn create_phase1(&self, _req: Phase1CreateRequest) -> Result<Phase1CreateResponse, ApiError> {
        Err(ApiError::new(
            501,
            "not_implemented",
            "two-phase create is not supported by this backend",
        ))
    }

    /// Multi-backend create phase 2: peer endpoints and multicast group.
    fn connect_phase2(
        &self,
        _req: Phase2ConnectRequest,
    ) -> Result<Phase2ConnectResponse, ApiError> {
        Err(ApiError::new(
            501,
            "not_implemented",
            "two-phase create is not supported by this backend",
        ))
    }
}
