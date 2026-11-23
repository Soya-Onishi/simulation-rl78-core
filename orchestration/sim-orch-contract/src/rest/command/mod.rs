//! Orchestrator ↔ Command REST surface (paths, request/reply bodies).

pub mod paths;
pub mod reply;
pub mod request;

pub use paths::{API_PREFIX, build_ensure, build_restart, build_status, build_terminate};
pub use paths::{node_command, probe_interest, reset, sessions, start, status, stop};
pub use reply::{CommandHostStatus, NodeProcessStatus, OrchestratorReply};
pub use request::{NodeCommandBody, ProbeInterestBody, StartBody};
