//! Per-edge pulse pub/sub (nodes `open_or_create` with topology QoS).

use std::collections::HashMap;

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::topology::{DirectedEdge, PayloadKind};

use sim_kernel::Pulse;

use super::names;
use super::runtime::IpcError;

/// Default pulse edge subscriber buffer.
///
/// Sized for waveform-parameter updates (not per-edge DigitalLevel toggles), so
/// a smaller ring than analog (256) is enough.
pub const DEFAULT_PULSE_RING_LEN: usize = 64;

type PulsePub = Publisher<ipc::Service, Pulse, ()>;
type PulseSub = Subscriber<ipc::Service, Pulse, ()>;

/// Node pulse endpoints for this board.
pub struct NodePulsePorts {
    pub producers: HashMap<crate::ipc::HashedId<u64>, PulsePub>,
    pub consumers: HashMap<crate::ipc::HashedId<u64>, PulseSub>,
}

impl NodePulsePorts {
    pub fn open_for_board(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_name: &str,
        topo_edges: &[DirectedEdge],
    ) -> Result<Self, IpcError> {
        let mut producers = HashMap::new();
        let mut consumers = HashMap::new();
        for edge in topo_edges {
            if edge.payload != PayloadKind::Pulse {
                continue;
            }
            let id = names::edge_id(
                &edge.from.path,
                &edge.from.port,
                &edge.to.path,
                &edge.to.port,
            );
            let svc_name = names::pulse_edge(cluster_key, id);
            if edge.from.path == board_name {
                let puber = open_or_create_pulse_publisher(node, &svc_name, DEFAULT_PULSE_RING_LEN)?;
                producers.insert(id, puber);
            } else if edge.to.path == board_name {
                let sub = open_or_create_pulse_subscriber(node, &svc_name, DEFAULT_PULSE_RING_LEN)?;
                consumers.insert(id, sub);
            }
        }
        Ok(Self {
            producers,
            consumers,
        })
    }
}

fn open_or_create_pulse_publisher(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<PulsePub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<Pulse>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create pulse pub {name}: {e:?}")))?;
    svc.publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("pulse publisher {name}: {e:?}")))
}

fn open_or_create_pulse_subscriber(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<PulseSub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<Pulse>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create pulse sub {name}: {e:?}")))?;
    svc.subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("pulse subscriber {name}: {e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::runtime::{create_node, isolated_config, new_cluster_key};
    use crate::topology::{
        BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, LogicalTopology, PayloadKind,
        PortRef,
    };
    use std::time::Duration;

    #[test]
    fn pulse_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let root = dir.path().join("iox");
        let config = isolated_config(&root).unwrap();
        let topo = LogicalTopology {
            boards: vec![
                BoardSpec {
                    id: "plant".into(),
                    kind: "plant".into(),
                    elf: None,
                    endpoints: vec![EndpointSpec {
                        name: "flow_pulse".into(),
                        direction: EndpointDirection::Out,
                        payload: PayloadKind::Pulse,
                    }],
                },
                BoardSpec {
                    id: "board".into(),
                    kind: "rl78".into(),
                    elf: None,
                    endpoints: vec![EndpointSpec {
                        name: "flow_pulse".into(),
                        direction: EndpointDirection::In,
                        payload: PayloadKind::Pulse,
                    }],
                },
            ],
            edges: vec![DirectedEdge {
                from: PortRef {
                    path: "plant".into(),
                    port: "flow_pulse".into(),
                },
                to: PortRef {
                    path: "board".into(),
                    port: "flow_pulse".into(),
                },
                route: vec![
                    PortRef {
                        path: "plant".into(),
                        port: "flow_pulse".into(),
                    },
                    PortRef {
                        path: "board".into(),
                        port: "flow_pulse".into(),
                    },
                ],
                payload: PayloadKind::Pulse,
                uart_ring_len: None,
            }],
            margin_ns: 1,
            booting_timeout_ms: 1000,
        };
        let na = create_node(&config, "pulse-plant").unwrap();
        let nb = create_node(&config, "pulse-board").unwrap();
        let porta = NodePulsePorts::open_for_board(&na, &key, "plant", &topo.edges).unwrap();
        let portb = NodePulsePorts::open_for_board(&nb, &key, "board", &topo.edges).unwrap();
        let edge = names::edge_id("plant", "flow_pulse", "board", "flow_pulse");
        let sample = Pulse {
            hi_width_ns: 1_000_000,
            lo_width_ns: 1_000_000,
        };
        porta
            .producers
            .get(&edge)
            .unwrap()
            .send_copy(sample)
            .unwrap();
        let mut got = None;
        for _ in 0..100 {
            if let Ok(Some(s)) = portb.consumers.get(&edge).unwrap().receive() {
                got = Some(*s);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, Some(sample));
    }
}
