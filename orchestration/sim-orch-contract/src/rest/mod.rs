//! REST contracts for orchestration peers.
//!
//! Each peer boundary gets its own submodule so request/reply types do not
//! pile into one `XxxToYyy` enum file. Today only Orchestrator ↔ Command is
//! defined here; Node control stays on iceoryx ([`crate::node_cmd`]).

pub mod command;
pub mod error;
pub mod node_dto;

pub use command::{
    COMMAND_API_PREFIX, CommandHostStatus, NodeProcessStatus, OrchestratorReply,
    OrchestratorRequest, PATH_BUILD_ENSURE, PATH_BUILD_RESTART, PATH_BUILD_STATUS,
    PATH_BUILD_TERMINATE, PATH_NODE_COMMAND, PATH_PROBE_INTEREST, PATH_RESET, PATH_SESSION,
    PATH_START, PATH_STATUS, PATH_STOP, build_status_url,
};
pub use error::{RestErrorBody, RestErrorCode};
pub use node_dto::{
    NodeCommandDto, NodeCommandDtoError, NodeCommandErrorDto, NodeCommandResultDto,
};
