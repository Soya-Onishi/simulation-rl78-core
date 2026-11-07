//! Multi-board cluster support: logical topology, singleton server, and
//! arbiter/board process helpers.
//!
//! Process IPC (iceoryx2 pub/sub) lives here so [`sim_kernel`] stays free of
//! transport dependencies. Architecture crates and the single-board CLI are
//! unchanged. Board/plant binaries use [`node!`] + [`board_runtime::run_node`]
//! for the shared cluster participation loop (see `rl78-minimal-board` /
//! `minimal-plant`).

pub mod action;
pub mod arbiter;
pub mod backend;
pub mod board_runtime;
pub mod cluster_stop;
pub mod config;
pub mod contract;
pub mod control;
pub mod data_plane;
pub mod elf_source;
pub mod gdb;
pub mod host;
pub mod http;
pub mod inspect;
pub mod ipc;
pub mod lifecycle;
pub mod log_msg;
pub mod node;
pub mod probe;
pub mod probe_hub;
mod probe_log;
pub mod server;
pub mod time_sync;
pub mod topology;

pub use arbiter::{ArbiterError, ArbiterOptions, run_arbiter, run_arbiter_from_path};
pub use backend::{LocalBackend, SimulationBackend, TcpipBackend};
pub use board_runtime::{
    BoardCliError, BoardError, BoardOptions, board_usage, cluster_host_stop_reason, list_catalog,
    parse_board_args, run_node,
};
pub use cluster_stop::is_cluster_relevant_reason;
pub use contract::{
    ActionList, AdvertisedAction, AdvertisedActionParam, ApiError, AttachGdbRequest,
    CreateSimulationRequest, CreateSimulationResponse, GdbSessionInfo, GdbSshCommand,
    InvokeActionRequest, LogEvent, ProbeSampleJson, ProbeSamplesResponse, StartSimulationRequest,
    StatusEvent, TopologyBoardView, TopologyEdgeView, TopologyEndpointView, TopologyPortRefView,
    TopologyView, prepare_create_topology,
};
pub use control::{ControlToArbiter, ControlToNode, HostStopReason};
pub use elf_source::ElfSourceError;
pub use lifecycle::{NodeEffect, NodeState, PeerEffect, PeerState};
pub use log_msg::{ClusterLog, LogConsole, LogLevel, LogOrigin};
pub use node::notify_host_stop;
pub use probe::{
    MAX_PROBES_PER_BOARD, NodeProbeRuntime, PROBE_NAME_CAP, ProbeAdvertisement, ProbeError,
    ProbePublishers, ProbeWireKind,
};
pub use probe_hub::{
    ArbiterProbeError, ArbiterProbeHub, ProbeCatalogEntry, ProbeSample, pack_probe_name,
    unpack_probe_name,
};
pub use server::{ServerError, ServerOptions, run_backend, spawn_worker};
pub use sim_kernel::{ProbePoint, ProbePolicy, ProbeTap, UartFrame};
pub use time_sync::TimeCeiling;
pub use topology::{
    BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, LogicalTopology, PayloadKind,
    PortRef, TopologyError, parse_logical_topology,
};
