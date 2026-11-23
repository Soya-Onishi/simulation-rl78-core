//! Shared contracts for the orchestration split (#141 / #139).
//!
//! Runtime loops live in `sim-command` / future `sim-node` / `sim-orchestrator`
//! crates. This crate defines layouts, names, and message shapes so
//! implementations share one protocol (no throwaway bridges).

pub mod endpoint;
pub mod id;
pub mod names;
pub mod node_cmd;
pub mod orchestrator2command;
pub mod probe;
pub mod shm;

pub use endpoint::{EndpointDatagram, EndpointPayload, EndpointWire};
pub use id::{ClusterId, HashedId, Hexable, ParseClusterIdError};
pub use names::{
    PROBE_SERVICE_SUFFIX, node_command_rr_path, node_endpoint_rx_path, probe_service_path,
};
pub use node_cmd::{
    INSPECT_CAP, MAX_NODE_PROBE_IDS, NodeCommand, NodeCommandError, NodeCommandResult,
};
pub use orchestrator2command::{
    API_PREFIX, CommandHostStatus, NodeCommandBody, NodeCommandDto, NodeCommandDtoError,
    NodeCommandErrorDto, NodeCommandResultDto, NodeProcessStatus, OrchestratorReply,
    ProbeInterestBody, RestErrorBody, RestErrorCode, StartBody, build_ensure, build_restart,
    build_status, build_terminate, node_command, probe_interest, reset, sessions, start, status,
    stop,
};
pub use probe::{DigitalLiveValue, DigitalProbeData, ProbeLiveSample, ProbeLogRecord};
pub use shm::{
    HOST_CTRL_FILE, HOST_CTRL_MAGIC, HOST_CTRL_VERSION, HostCtrlAtoms, HostCtrlHeader,
    HostCtrlLayout, STOP_GEN_SATURATED,
};
