//! Shared contracts for the orchestration split (#141 / #139).
//!
//! Runtime loops live in `sim-command` / `sim-node` / future `sim-orchestrator`
//! crates. This crate defines layouts, names, and message shapes so
//! implementations share one protocol (no throwaway bridges).

pub mod command2node;
pub mod endpoint;
pub mod id;
pub mod ipc;
pub mod names;
pub mod node2orchestrator;
pub mod orchestrator2command;
pub mod probe;
pub mod shm;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod threads;

pub use command2node::launch::{NodeArgsError, NodeConfig};
pub use command2node::rr::{
    NodeCommandClient, NodeCommandServer, NodeCommandService, NodeRequestError,
    create_node_command_server, open_node_command_service,
};
pub use command2node::{
    INSPECT_CAP, MAX_NODE_PROBE_IDS, NodeCommand, NodeCommandError, NodeCommandResult,
};
pub use endpoint::{EndpointDatagram, EndpointPayload, EndpointWire};
pub use id::{ClusterId, HashedId, Hexable, ParseClusterIdError};
pub use ipc::{IoxNode, IpcError, create_iox_node, isolated_config};
pub use names::{
    PROBE_SERVICE_SUFFIX, node_command_rr_path, node_endpoint_rx_path, probe_service_path,
};
pub use node2orchestrator::{NodeLogLevel, NodeLogRecord};
pub use orchestrator2command::{
    API_PREFIX, CommandHostStatus, NodeCommandBody, NodeCommandDto, NodeCommandDtoError,
    NodeCommandErrorDto, NodeCommandResultDto, NodeProcessStatus, OrchestratorReply,
    ProbeInterestBody, RestErrorBody, RestErrorCode, StartBody, build_ensure, build_restart,
    build_status, build_terminate, node_command, probe_interest, reset, sessions, start, status,
    stop,
};
pub use probe::{DigitalLiveValue, DigitalProbeData, ProbeLiveSample, ProbeLogRecord};
pub use shm::{
    HOST_CTRL_FILE, HOST_CTRL_MAGIC, HOST_CTRL_VERSION, HostCtrl, HostCtrlAtoms, HostCtrlHeader,
    HostCtrlLayout, HostCtrlLease, STOP_GEN_SATURATED, host_ctrl_path,
};
pub use threads::{ThreadGroup, ThreadPanicked};
