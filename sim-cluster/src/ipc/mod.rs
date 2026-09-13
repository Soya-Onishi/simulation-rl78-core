//! iceoryx2-backed cluster IPC (control + UART pub/sub).

mod control_bus;
mod hash;
mod names;
mod runtime;
mod uart_bus;
mod wire;

pub use control_bus::{ArbiterControl, NodeControl};
pub use hash::{board_hash_table, board_id_hash};
pub use names::{ctrl_a2n, ctrl_n2a, edge_id, uart_edge};
pub use runtime::{
    create_node, isolated_config, new_cluster_key, root_path_for_cluster, IpcError,
};
pub use uart_bus::{NodeUartPorts, UartServicesCreated};
pub use wire::{ControlWire, HostStopReason, UartFrame};
