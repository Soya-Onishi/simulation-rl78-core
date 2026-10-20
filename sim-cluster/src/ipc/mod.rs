//! iceoryx2-backed cluster IPC (control, log, UART, and probe pub/sub).

mod action_bus;
mod control_bus;
mod hash;
mod host_bus;
mod log_bus;
mod names;
mod probe_bus;
mod probe_forward;
mod runtime;
mod uart_bus;

pub use action_bus::{ArbiterNodeActions, ArbiterServerActions, NodeActions, ServerActions};
pub use control_bus::{ArbiterControl, NodeControl};
pub use hash::{board_hash_table, board_id_hash, probe_id_hash, probe_name_hash};
pub use host_bus::{ArbiterHostPort, ServerHostPort};
pub use log_bus::{ArbiterLogBus, NodeLog, ServerLogInbox};
pub use names::{
    ctrl_a2n, ctrl_n2a, ctrl_s2a, edge_id, log_a2s, log_n2a, probe as probe_service_name,
    status_a2s, uart_edge,
};
pub use probe_bus::{
    ProbeDigitalPub, ProbeDigitalSample, ProbeDigitalSub, ProbeUartPub, ProbeUartSample,
    ProbeUartSub, open_digital_publisher, open_digital_subscriber, open_uart_publisher,
    open_uart_subscriber, send_digital, send_uart,
};
pub use probe_forward::{
    ArbiterProbeForward, MSG_CATALOG, MSG_SAMPLE, ProbeToServer, ServerProbeInbox,
};
pub use runtime::{IpcError, create_node, isolated_config, new_cluster_key, root_path_for_cluster};
pub use uart_bus::NodeUartPorts;
