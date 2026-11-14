//! Per-edge scalar (`f64`) pub/sub (nodes `open_or_create` with topology QoS).

use std::collections::HashMap;

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::topology::{DirectedEdge, PayloadKind};

use sim_kernel::Scalar;

use super::names;
use super::runtime::IpcError;

/// Default scalar edge subscriber buffer.
///
/// Matches analog (256): plant quantities (temperature, flow) update on the
/// plant step, not as rare waveform-parameter changes like pulse (64).
pub const DEFAULT_SCALAR_RING_LEN: usize = 256;

type ScalarPub = Publisher<ipc::Service, Scalar, ()>;
type ScalarSub = Subscriber<ipc::Service, Scalar, ()>;

/// Node scalar endpoints for this board.
pub struct NodeScalarPorts {
    pub producers: HashMap<crate::ipc::HashedId<u64>, ScalarPub>,
    pub consumers: HashMap<crate::ipc::HashedId<u64>, ScalarSub>,
}

impl NodeScalarPorts {
    pub fn open_for_board(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_name: &str,
        topo_edges: &[DirectedEdge],
    ) -> Result<Self, IpcError> {
        let mut producers = HashMap::new();
        let mut consumers = HashMap::new();
        for edge in topo_edges {
            if edge.payload != PayloadKind::Scalar {
                continue;
            }
            let id = names::edge_id(
                &edge.from.path,
                &edge.from.port,
                &edge.to.path,
                &edge.to.port,
            );
            let svc_name = names::scalar_edge(cluster_key, id);
            if edge.from.path == board_name {
                let puber =
                    open_or_create_scalar_publisher(node, &svc_name, DEFAULT_SCALAR_RING_LEN)?;
                producers.insert(id, puber);
            } else if edge.to.path == board_name {
                let sub =
                    open_or_create_scalar_subscriber(node, &svc_name, DEFAULT_SCALAR_RING_LEN)?;
                consumers.insert(id, sub);
            }
        }
        Ok(Self {
            producers,
            consumers,
        })
    }
}

fn open_or_create_scalar_publisher(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<ScalarPub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<Scalar>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create scalar pub {name}: {e:?}")))?;
    svc.publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("scalar publisher {name}: {e:?}")))
}

fn open_or_create_scalar_subscriber(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<ScalarSub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<Scalar>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create scalar sub {name}: {e:?}")))?;
    svc.subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("scalar subscriber {name}: {e:?}")))
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
    fn scalar_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let root = dir.path().join("iox");
        let config = isolated_config(&root).unwrap();
        let topo = LogicalTopology {
            boards: vec![
                BoardSpec {
                    id: "piping".into(),
                    kind: "piping-plant".into(),
                    elf: None,
                    endpoints: vec![EndpointSpec {
                        name: "inlet_temp_c".into(),
                        direction: EndpointDirection::Out,
                        payload: PayloadKind::Scalar,
                    }],
                },
                BoardSpec {
                    id: "boiler".into(),
                    kind: "boiler-plant".into(),
                    elf: None,
                    endpoints: vec![EndpointSpec {
                        name: "inlet_temp_c".into(),
                        direction: EndpointDirection::In,
                        payload: PayloadKind::Scalar,
                    }],
                },
            ],
            edges: vec![DirectedEdge {
                from: PortRef {
                    path: "piping".into(),
                    port: "inlet_temp_c".into(),
                },
                to: PortRef {
                    path: "boiler".into(),
                    port: "inlet_temp_c".into(),
                },
                route: vec![
                    PortRef {
                        path: "piping".into(),
                        port: "inlet_temp_c".into(),
                    },
                    PortRef {
                        path: "boiler".into(),
                        port: "inlet_temp_c".into(),
                    },
                ],
                payload: PayloadKind::Scalar,
                uart_ring_len: None,
            }],
            margin_ns: 1,
            booting_timeout_ms: 1000,
        };
        let na = create_node(&config, "scalar-piping").unwrap();
        let nb = create_node(&config, "scalar-boiler").unwrap();
        let porta = NodeScalarPorts::open_for_board(&na, &key, "piping", &topo.edges).unwrap();
        let portb = NodeScalarPorts::open_for_board(&nb, &key, "boiler", &topo.edges).unwrap();
        let edge = names::edge_id("piping", "inlet_temp_c", "boiler", "inlet_temp_c");
        let sample = Scalar(42.5);
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
