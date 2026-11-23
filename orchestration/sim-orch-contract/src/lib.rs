//! Shared contracts for the orchestration split (#141 / #139).
//!
//! Runtime loops live in `sim-command` / future `sim-node` / `sim-orchestrator`
//! crates. This crate defines layouts, names, and message shapes so
//! implementations share one protocol (no throwaway bridges).

pub mod cluster_id;
pub mod endpoint;
pub mod hashed_id;
pub mod names;
pub mod node_cmd;
pub mod probe_log;
pub mod probe_wire;
pub mod rest;
pub mod shm;

pub use cluster_id::{ClusterId, ParseClusterIdError};
pub use endpoint::{EndpointDatagram, EndpointPayload, EndpointWire};
pub use hashed_id::{HashedId, Hexable};
pub use names::{
    PROBE_SERVICE_SUFFIX, node_command_rr_path, node_endpoint_rx_path, probe_service_path,
};
pub use node_cmd::{
    INSPECT_CAP, MAX_NODE_PROBE_IDS, NodeCommand, NodeCommandError, NodeCommandResult,
};
pub use probe_log::{DigitalProbeData, ProbeLogRecord};
pub use probe_wire::{DigitalLiveValue, ProbeLiveSample};
pub use rest::{
    API_PREFIX, CommandHostStatus, NodeCommandBody, NodeCommandDto, NodeCommandDtoError,
    NodeCommandErrorDto, NodeCommandResultDto, NodeProcessStatus, OrchestratorReply,
    ProbeInterestBody, RestErrorBody, RestErrorCode, StartBody, build_ensure, build_restart,
    build_status, build_terminate, node_command, probe_interest, reset, sessions, start, status,
    stop,
};
pub use shm::{
    HOST_CTRL_FILE, HOST_CTRL_MAGIC, HOST_CTRL_VERSION, HostCtrlAtoms, HostCtrlHeader,
    HostCtrlLayout, STOP_GEN_SATURATED,
};
