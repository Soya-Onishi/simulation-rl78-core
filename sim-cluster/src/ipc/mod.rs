//! Cluster IPC: iceoryx2 for arbiter↔node, in-process mpsc for server↔arbiter.

mod action_bus;
mod analog_bus;
mod control_bus;
mod hash;
mod log_bus;
mod names;
mod probe_bus;
mod probe_ctrl;
mod probe_forward;
mod runtime;
pub(crate) mod server_arbiter;
mod uart_bus;

pub use action_bus::{ArbiterNodeActions, NodeActions};
pub use analog_bus::NodeAnalogPorts;
pub use control_bus::{ArbiterControl, NodeControl};
pub use hash::{
    HashedId, Hexable, board_hash_table, board_id_hash, edge_hash_table, edge_id_hash,
    path_hash_table, port_hash_table, port_id_hash, probe_id_hash, probe_name_hash,
};
pub use log_bus::{ArbiterLogBus, NodeLog};
pub use names::{
    action_a2n, action_n2a, analog_edge, ctrl_a2n, ctrl_n2a, edge_id, edge_id_logical, log_n2a,
    probe as probe_service_name, probe_csv_a2n, probe_csv_n2a, probe_query_a2n, probe_query_n2a,
    uart_edge,
};
pub use probe_bus::{
    ProbeAnalogPub, ProbeAnalogSample, ProbeAnalogSub, ProbeDigitalPub, ProbeDigitalSample,
    ProbeDigitalSub, ProbeUartPub, ProbeUartSample, ProbeUartSub, open_analog_publisher,
    open_analog_subscriber, open_digital_publisher, open_digital_subscriber, open_uart_publisher,
    open_uart_subscriber, send_analog, send_digital, send_uart,
};
pub use probe_ctrl::{
    ArbiterProbeCtrl, CSV_CHUNKS_PER_TICK, NodeProbeCsv, NodeProbeQuery, ProbeQueryReplyWire,
    ProbeQueryWire, ProbeWatchWire, QUERY_CHUNK, QUERY_KIND_LATEST, QUERY_KIND_RANGE,
    QUERY_REPLY_LAST, QUERY_REPLY_TRUNCATED,
};
pub use probe_forward::{MSG_CATALOG, MSG_CATALOG_END, MSG_SAMPLE, ProbeToServer};
pub use runtime::{IpcError, create_node, isolated_config, new_cluster_key, root_path_for_cluster};
pub use server_arbiter::{
    ArbiterHostPort, ArbiterLink, ArbiterProbeForward, ArbiterServerActions, ServerActions,
    ServerHostPort, ServerLink, ServerLogInbox, ServerProbeCtrl, ServerProbeInbox, open_pair,
};
pub use uart_bus::NodeUartPorts;
