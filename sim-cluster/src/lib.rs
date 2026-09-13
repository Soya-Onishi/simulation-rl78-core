//! Multi-board cluster support: logical topology, singleton server, and
//! arbiter/node process helpers.
//!
//! Process IPC (iceoryx2 pub/sub) lives here so [`sim_kernel`] stays free of
//! transport dependencies. Architecture crates and the single-board CLI are
//! unchanged.

pub mod arbiter;
pub mod cluster_stop;
pub mod control;
pub mod ipc;
pub mod lifecycle;
pub mod node;
pub mod server;
pub mod time_sync;
pub mod topology;

pub use arbiter::{ArbiterError, ArbiterOptions, run_arbiter, run_arbiter_from_path};
pub use cluster_stop::is_cluster_relevant_reason;
pub use control::{BoardTarget, ControlToArbiter, ControlToNode, HostStopReason};
pub use ipc::UartFrame;
pub use lifecycle::{NodeEffect, NodeState, PeerEffect, PeerState};
pub use node::{InjectHostStop, NodeError, NodeOptions, notify_host_stop, run_node};
pub use server::{ServerError, ServerOptions, run_server};
pub use time_sync::TimeCeiling;
pub use topology::{
    BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, LogicalTopology, PayloadKind,
    TopologyError, parse_logical_topology,
};
