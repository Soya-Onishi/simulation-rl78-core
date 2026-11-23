//! Orchestrator ↔ Command REST surface (paths, request/reply bodies, errors).

pub mod error;
pub mod paths;
pub mod reply;
pub mod request;

pub use error::{RestErrorBody, RestErrorCode};
pub use paths::{
    API_PREFIX, build_ensure, build_restart, build_status, build_terminate, node_command,
    probe_interest, reset, sessions, start, status, stop,
};
pub use reply::{
    CommandHostStatus, NodeCommandErrorDto, NodeCommandResultDto, NodeProcessStatus,
    OrchestratorReply,
};
pub use request::{
    NodeCommandBody, NodeCommandDto, NodeCommandDtoError, ProbeInterestBody, StartBody,
};
