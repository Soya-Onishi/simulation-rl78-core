//! Per-edge analog pub/sub (nodes `open_or_create` with topology QoS).

use std::collections::HashMap;

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::topology::{DirectedEdge, PayloadKind};

use sim_kernel::AnalogVoltage;

use super::names;
use super::runtime::IpcError;

/// Default analog edge subscriber buffer.
pub const DEFAULT_ANALOG_RING_LEN: usize = 256;

type AnalogPub = Publisher<ipc::Service, AnalogVoltage, ()>;
type AnalogSub = Subscriber<ipc::Service, AnalogVoltage, ()>;

/// Node analog endpoints for this board.
pub struct NodeAnalogPorts {
    pub producers: HashMap<String, AnalogPub>,
    pub consumers: HashMap<String, AnalogSub>,
}

impl NodeAnalogPorts {
    pub fn open_for_board(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_id: &str,
        topo_edges: &[DirectedEdge],
    ) -> Result<Self, IpcError> {
        let mut producers = HashMap::new();
        let mut consumers = HashMap::new();
        for edge in topo_edges {
            if edge.payload != PayloadKind::Analog {
                continue;
            }
            let id = names::edge_id(
                &edge.from_board,
                &edge.from_endpoint,
                &edge.to_board,
                &edge.to_endpoint,
            );
            let svc_name = names::analog_edge(cluster_key, &id);
            if edge.from_board == board_id {
                let puber =
                    open_or_create_analog_publisher(node, &svc_name, DEFAULT_ANALOG_RING_LEN)?;
                producers.insert(id, puber);
            } else if edge.to_board == board_id {
                let sub =
                    open_or_create_analog_subscriber(node, &svc_name, DEFAULT_ANALOG_RING_LEN)?;
                consumers.insert(id, sub);
            }
        }
        Ok(Self {
            producers,
            consumers,
        })
    }
}

fn open_or_create_analog_publisher(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<AnalogPub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<AnalogVoltage>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create analog pub {name}: {e:?}")))?;
    svc.publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("analog publisher {name}: {e:?}")))
}

fn open_or_create_analog_subscriber(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<AnalogSub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<AnalogVoltage>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create analog sub {name}: {e:?}")))?;
    svc.subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("analog subscriber {name}: {e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::runtime::{create_node, isolated_config, new_cluster_key};
    use crate::topology::{
        BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, LogicalTopology,
    };
    use std::time::Duration;

    #[test]
    fn analog_voltage_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let root = dir.path().join("iox");
        let config = isolated_config(&root).unwrap();
        let topo = LogicalTopology {
            boards: vec![
                BoardSpec {
                    id: "a".into(),
                    kind: "rl78".into(),
                    elf: None,
                    endpoints: vec![EndpointSpec {
                        name: "dac0".into(),
                        direction: EndpointDirection::Out,
                        payload: PayloadKind::Analog,
                    }],
                },
                BoardSpec {
                    id: "p".into(),
                    kind: "rl78".into(),
                    elf: None,
                    endpoints: vec![EndpointSpec {
                        name: "vin".into(),
                        direction: EndpointDirection::In,
                        payload: PayloadKind::Analog,
                    }],
                },
            ],
            edges: vec![DirectedEdge {
                from_board: "a".into(),
                from_endpoint: "dac0".into(),
                to_board: "p".into(),
                to_endpoint: "vin".into(),
                payload: PayloadKind::Analog,
                uart_ring_len: None,
            }],
            margin_ns: 1,
            headroom_threshold_ns: 1,
            ready_timeout_ms: 1000,
        };
        let na = create_node(&config, "analog-a").unwrap();
        let np = create_node(&config, "analog-p").unwrap();
        let porta = NodeAnalogPorts::open_for_board(&na, &key, "a", &topo.edges).unwrap();
        let portp = NodeAnalogPorts::open_for_board(&np, &key, "p", &topo.edges).unwrap();
        let edge = names::edge_id("a", "dac0", "p", "vin");
        let sample = AnalogVoltage(5_000_000);
        porta
            .producers
            .get(&edge)
            .unwrap()
            .send_copy(sample)
            .unwrap();
        let mut got = None;
        for _ in 0..100 {
            if let Ok(Some(s)) = portp.consumers.get(&edge).unwrap().receive() {
                got = Some(*s);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, Some(sample));
    }
}
