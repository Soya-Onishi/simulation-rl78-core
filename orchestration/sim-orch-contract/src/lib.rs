//! Shared contracts for the orchestration split (#141 / #139).
//!
//! Runtime loops live in future `sim-node` / `sim-command` / `sim-orchestrator`
//! crates. This crate defines layouts, names, and message shapes so
//! implementations share one protocol (no throwaway bridges).

pub mod endpoint;
pub mod names;
pub mod node_cmd;
pub mod probe_log;
pub mod probe_wire;
pub mod rest;
pub mod shm;

pub use endpoint::{EndpointPayload, EndpointWire};
pub use names::{
    PROBE_SERVICE_SUFFIX, node_command_rr_path, node_endpoint_rx_path, probe_service_path,
};
pub use node_cmd::{
    INSPECT_CAP, MAX_NODE_PROBE_IDS, NodeCommand, NodeCommandError, NodeCommandResult,
};
pub use probe_log::{DigitalProbeData, ProbeLogRecord};
pub use probe_wire::ProbeLiveSample;
pub use rest::{
    COMMAND_API_PREFIX, CommandHostStatus, NodeCommandDto, NodeCommandDtoError,
    NodeCommandErrorDto, NodeCommandResultDto, NodeProcessStatus, OrchToCommand,
    OrchToCommandReply, RestErrorBody, RestErrorCode,
};
pub use shm::{
    HOST_CTRL_FILE, HOST_CTRL_MAGIC, HOST_CTRL_VERSION, HostCtrlAtoms, HostCtrlHeader,
    HostCtrlLayout, STOP_GEN_SATURATED, bump_stop_generation, saturating_bump_stop_gen,
};
