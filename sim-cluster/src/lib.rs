//! Multi-board cluster support: logical topology, singleton server, and
//! arbiter/node process helpers.
//!
//! Process IPC (UDS / SHM) lives here so [`sim_kernel`] stays free of transport
//! dependencies. Architecture crates and the single-board CLI are unchanged.

pub mod arbiter;
pub mod blit;
pub mod cluster_stop;
pub mod control;
pub mod lifecycle;
pub mod node;
pub mod server;
pub mod shm_uart;
pub mod time_sync;
pub mod topology;

pub use arbiter::{ArbiterError, ArbiterOptions, run_arbiter, run_arbiter_from_path};
pub use blit::{UartInbox, UartRxThread};
pub use cluster_stop::is_cluster_relevant_reason;
pub use control::{ControlMessage, ShmBinding, ShmRole};
pub use lifecycle::{NodeEffect, NodeState, PeerEffect, PeerState};
pub use node::{
    InjectHostStop, NodeError, NodeOptions, notify_host_stop, run_node, run_node_with_options,
};
pub use server::{ServerError, ServerOptions, run_server};
pub use shm_uart::{ShmUartError, ShmUartFrame, UartShmEndpoint, UartShmOwner};
pub use time_sync::TimeCeiling;
pub use topology::{
    BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, LogicalTopology, PayloadKind,
    TopologyError, parse_logical_topology,
};
