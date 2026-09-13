//! Per-edge UART pub/sub (nodes `open_or_create` with topology QoS).

use std::collections::HashMap;

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::topology::{DirectedEdge, LogicalTopology, PayloadKind};

use super::names;
use super::runtime::IpcError;
use super::wire::UartFrame;

type UartPub = Publisher<ipc::Service, UartFrame, ()>;
type UartSub = Subscriber<ipc::Service, UartFrame, ()>;

/// Node UART endpoints for this board.
///
/// TX and RX both use `open_or_create` with identical SPSC QoS derived from the
/// topology; whichever side arrives first creates the service and holds the port.
pub struct NodeUartPorts {
    pub producers: HashMap<String, UartPub>,
    pub consumers: HashMap<String, UartSub>,
}

impl NodeUartPorts {
    pub fn open_for_board(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_id: &str,
        topo_edges: &[DirectedEdge],
    ) -> Result<Self, IpcError> {
        let mut producers = HashMap::new();
        let mut consumers = HashMap::new();
        for edge in topo_edges {
            if edge.payload != PayloadKind::Uart {
                continue;
            }
            let id = names::edge_id(
                &edge.from_board,
                &edge.from_endpoint,
                &edge.to_board,
                &edge.to_endpoint,
            );
            let svc_name = names::uart_edge(cluster_key, &id);
            let capacity = LogicalTopology::uart_ring_len(edge).max(2) as usize;
            if edge.from_board == board_id {
                let puber = open_or_create_uart_publisher(node, &svc_name, capacity)?;
                producers.insert(id, puber);
            } else if edge.to_board == board_id {
                let sub = open_or_create_uart_subscriber(node, &svc_name, capacity)?;
                consumers.insert(id, sub);
            }
        }
        Ok(Self {
            producers,
            consumers,
        })
    }

    pub fn try_send(&self, edge_id: &str, frame: UartFrame) -> Result<(), IpcError> {
        let Some(puber) = self.producers.get(edge_id) else {
            return Err(IpcError::Message(format!("no producer for {edge_id}")));
        };
        puber
            .send_copy(frame)
            .map_err(|e| IpcError::Message(format!("uart send {edge_id}: {e:?}")))?;
        Ok(())
    }

    /// Non-blocking drain of all consumer edges.
    pub fn drain_all(&self, limit_per_edge: usize) -> Result<Vec<(String, Vec<UartFrame>)>, IpcError> {
        let mut out = Vec::new();
        for (id, sub) in &self.consumers {
            let mut frames = Vec::new();
            for _ in 0..limit_per_edge {
                match sub
                    .receive()
                    .map_err(|e| IpcError::Message(format!("uart recv {id}: {e:?}")))?
                {
                    Some(sample) => frames.push(*sample),
                    None => break,
                }
            }
            if !frames.is_empty() {
                out.push((id.clone(), frames));
            }
        }
        Ok(out)
    }
}

fn open_or_create_uart_publisher(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<UartPub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<UartFrame>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create uart pub {name}: {e:?}")))?;
    svc.publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("uart publisher {name}: {e:?}")))
}

fn open_or_create_uart_subscriber(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<UartSub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<UartFrame>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create uart sub {name}: {e:?}")))?;
    svc.subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("uart subscriber {name}: {e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::runtime::{create_node, isolated_config, new_cluster_key};
    use crate::topology::{BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, PayloadKind};
    use std::time::Duration;

    #[test]
    fn uart_frame_round_trip() {
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
                        name: "tx".into(),
                        direction: EndpointDirection::Out,
                        payload: PayloadKind::Uart,
                    }],
                },
                BoardSpec {
                    id: "b".into(),
                    kind: "rl78".into(),
                    elf: None,
                    endpoints: vec![EndpointSpec {
                        name: "rx".into(),
                        direction: EndpointDirection::In,
                        payload: PayloadKind::Uart,
                    }],
                },
            ],
            edges: vec![DirectedEdge {
                from_board: "a".into(),
                from_endpoint: "tx".into(),
                to_board: "b".into(),
                to_endpoint: "rx".into(),
                payload: PayloadKind::Uart,
                uart_ring_len: Some(8),
            }],
            margin_ns: 1,
            headroom_threshold_ns: 1,
            ready_timeout_ms: 1000,
        };
        // No arbiter-side create: TX/RX nodes open_or_create themselves.
        let na = create_node(&config, "uart-a").unwrap();
        let nb = create_node(&config, "uart-b").unwrap();
        let porta = NodeUartPorts::open_for_board(&na, &key, "a", &topo.edges).unwrap();
        let portb = NodeUartPorts::open_for_board(&nb, &key, "b", &topo.edges).unwrap();
        let edge = names::edge_id("a", "tx", "b", "rx");
        let frame = UartFrame {
            data: 0x5A,
            data_bits: 8,
            ..UartFrame::default()
        };
        porta.try_send(&edge, frame).unwrap();
        let mut got = None;
        for _ in 0..100 {
            let drained = portb.drain_all(8).unwrap();
            if let Some((_, frames)) = drained.into_iter().next() {
                got = frames.into_iter().next();
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, Some(frame));
    }
}
