//! Multi-board cluster support: logical topology, singleton server, and
//! arbiter/board process helpers.
//!
//! Process IPC (iceoryx2 pub/sub) lives here so [`sim_kernel`] stays free of
//! transport dependencies. Architecture crates and the single-board CLI are
//! unchanged. Board binaries use [`board_runtime::run_board`] for the shared
//! cluster participation loop (see `rl78-minimal-board`).

pub mod arbiter;
pub mod board_runtime;
pub mod cluster_stop;
pub mod control;
pub mod data_plane;
pub mod ipc;
pub mod lifecycle;
pub mod node;
pub mod server;
pub mod time_sync;
pub mod topology;

pub use arbiter::{ArbiterError, ArbiterOptions, run_arbiter, run_arbiter_from_path};
pub use board_runtime::{
    BoardCliError, BoardError, BoardOptions, board_usage, cluster_host_stop_reason,
    parse_board_args, run_board,
};
pub use cluster_stop::is_cluster_relevant_reason;
pub use control::{ControlToArbiter, ControlToNode, HostStopReason};
pub use lifecycle::{NodeEffect, NodeState, PeerEffect, PeerState};
pub use node::notify_host_stop;
pub use server::{ServerError, ServerOptions, run_server};
pub use sim_kernel::UartFrame;
pub use time_sync::TimeCeiling;
pub use topology::{
    BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, LogicalTopology, PayloadKind,
    TopologyError, parse_logical_topology,
};
