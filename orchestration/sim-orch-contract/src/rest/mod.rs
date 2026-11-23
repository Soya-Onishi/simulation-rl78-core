//! REST contracts for orchestration peers.
//!
//! Each peer boundary gets its own submodule so request/reply types do not
//! pile into one `XxxToYyy` enum file. Today only Orchestrator ↔ Command is
//! defined here; Node control stays on iceoryx ([`crate::node_cmd`]).

pub mod command;
pub mod error;
pub mod node_dto;

pub use command::{
    API_PREFIX, CommandHostStatus, NodeCommandBody, NodeProcessStatus, OrchestratorReply,
    ProbeInterestBody, StartBody, build_ensure, build_restart, build_status, build_terminate,
    node_command, probe_interest, reset, sessions, start, status, stop,
};
pub use error::{RestErrorBody, RestErrorCode};
pub use node_dto::{
    NodeCommandDto, NodeCommandDtoError, NodeCommandErrorDto, NodeCommandResultDto,
};
