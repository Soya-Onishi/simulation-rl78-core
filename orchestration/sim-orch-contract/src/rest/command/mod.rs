//! Orchestrator ↔ Command REST surface (paths, request/reply bodies).

pub mod paths;
pub mod reply;
pub mod request;

pub use paths::{
    COMMAND_API_PREFIX, PATH_BUILD_ENSURE, PATH_BUILD_RESTART, PATH_BUILD_STATUS,
    PATH_BUILD_TERMINATE, PATH_NODE_COMMAND, PATH_PROBE_INTEREST, PATH_RESET, PATH_SESSION,
    PATH_START, PATH_STATUS, PATH_STOP, build_status_url,
};
pub use reply::{CommandHostStatus, NodeProcessStatus, OrchestratorReply};
pub use request::OrchestratorRequest;
