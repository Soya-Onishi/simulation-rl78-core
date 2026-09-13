//! Per-edge UART pub/sub.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;
use iceoryx2::service::port_factory::publish_subscribe::PortFactory;

use crate::topology::{DirectedEdge, LogicalTopology, PayloadKind};

use super::names;
use super::runtime::IpcError;
use super::wire::UartFrame;

type UartPub = Publisher<ipc::Service, UartFrame, ()>;
type UartSub = Subscriber<ipc::Service, UartFrame, ()>;
type UartFactory = PortFactory<ipc::Service, UartFrame, ()>;

/// Arbiter-held UART service factories (must stay alive for the cluster lifetime).
pub struct UartServicesCreated {
    /// Edge ids that were created.
    pub edge_ids: Vec<String>,
    _factories: Vec<UartFactory>,
}

impl UartServicesCreated {
    /// Create one pubsub service per UART directed edge (SPSC QoS).
    pub fn create_all(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        topo: &LogicalTopology,
    ) -> Result<Self, IpcError> {
        let mut edge_ids = Vec::new();
        let mut factories = Vec::new();
        for edge in &topo.edges {
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
            let factory = node
                .service_builder(
                    &svc_name
                        .as_str()
                        .try_into()
                        .map_err(|e| IpcError::Message(format!("bad uart name {svc_name}: {e:?}")))?,
                )
                .publish_subscribe::<UartFrame>()
                .max_publishers(1)
                .max_subscribers(1)
                .max_nodes(16)
                .subscriber_max_buffer_size(capacity)
                .enable_safe_overflow(true)
                .create()
                .map_err(|e| IpcError::Message(format!("create uart {svc_name}: {e:?}")))?;
            edge_ids.push(id);
            factories.push(factory);
        }
        Ok(Self {
            edge_ids,
            _factories: factories,
        })
    }
}

/// Node UART endpoints for this board.
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
        open_budget: Duration,
    ) -> Result<Self, IpcError> {
        let deadline = Instant::now() + open_budget;
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
            if edge.from_board == board_id {
                let puber = open_uart_publisher(node, &svc_name, deadline)?;
                producers.insert(id, puber);
            } else if edge.to_board == board_id {
                let sub = open_uart_subscriber(node, &svc_name, deadline)?;
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

fn open_uart_publisher(
    node: &Node<ipc::Service>,
    name: &str,
    deadline: Instant,
) -> Result<UartPub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    loop {
        match node
            .service_builder(&svc_name)
            .publish_subscribe::<UartFrame>()
            .open()
        {
            Ok(svc) => {
                return svc
                    .publisher_builder()
                    .create()
                    .map_err(|e| IpcError::Message(format!("uart publisher {name}: {e:?}")));
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(IpcError::Message(format!("open uart pub {name}: {e:?}"))),
        }
    }
}

fn open_uart_subscriber(
    node: &Node<ipc::Service>,
    name: &str,
    deadline: Instant,
) -> Result<UartSub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    loop {
        match node
            .service_builder(&svc_name)
            .publish_subscribe::<UartFrame>()
            .open()
        {
            Ok(svc) => {
                return svc
                    .subscriber_builder()
                    .create()
                    .map_err(|e| IpcError::Message(format!("uart subscriber {name}: {e:?}")));
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(IpcError::Message(format!("open uart sub {name}: {e:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::runtime::{create_node, isolated_config, new_cluster_key};
    use crate::topology::{BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, PayloadKind};

    #[test]
    fn uart_frame_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let root = dir.path().join("iox");
        let config = isolated_config(&root).unwrap();
        let arb = create_node(&config, "uart-arb").unwrap();
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
        let _svc = UartServicesCreated::create_all(&arb, &key, &topo).unwrap();
        let na = create_node(&config, "uart-a").unwrap();
        let nb = create_node(&config, "uart-b").unwrap();
        let porta =
            NodeUartPorts::open_for_board(&na, &key, "a", &topo.edges, Duration::from_secs(2))
                .unwrap();
        let portb =
            NodeUartPorts::open_for_board(&nb, &key, "b", &topo.edges, Duration::from_secs(2))
                .unwrap();
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
