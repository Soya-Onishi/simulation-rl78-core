//! Cross-host board-edge bridge: local iceoryx ↔ cluster multicast (p2).
//!
//! Same-host edges stay peer iceoryx. For a local→remote edge the arbiter
//! subscribes and multicasts; for remote→local it publishes samples received
//! over multicast onto the edge service the RX board already opens.

use std::collections::{HashMap, HashSet};

use iceoryx2::prelude::*;

use crate::cluster_mcast::{
    ClusterMulticast, MulticastMessage, decode_analog_payload, decode_pulse_payload,
    decode_scalar_payload, decode_uart_payload, encode_analog_payload, encode_pulse_payload,
    encode_scalar_payload, encode_uart_payload, endpoint_payload_kind_wire,
};
use crate::config::MachineId;
use crate::ipc::{
    AnalogPub, AnalogSub, DEFAULT_ANALOG_RING_LEN, DEFAULT_PULSE_RING_LEN, DEFAULT_SCALAR_RING_LEN,
    HashedId, IpcError, PulsePub, PulseSub, ScalarPub, ScalarSub, UartPub, UartSub, analog_edge,
    edge_id, open_or_create_analog_publisher, open_or_create_analog_subscriber,
    open_or_create_pulse_publisher, open_or_create_pulse_subscriber,
    open_or_create_scalar_publisher, open_or_create_scalar_subscriber,
    open_or_create_uart_publisher, open_or_create_uart_subscriber, pulse_edge, scalar_edge,
    uart_edge,
};
use crate::topology::{DirectedEdge, LogicalTopology, PayloadKind};

/// Bounded iceoryx drain per egress edge per arbiter tick.
pub const EGRESS_DRAIN_LIMIT: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeRole {
    /// Local TX → remote RX: subscribe iceoryx, multicast samples.
    Egress,
    /// Remote TX → local RX: multicast → publish iceoryx.
    Ingress,
}

/// Classify one edge relative to this host's local board set.
#[must_use]
pub fn classify_edge(edge: &DirectedEdge, local_boards: &HashSet<String>) -> Option<BridgeRole> {
    endpoint_payload_kind_wire(edge.payload)?;
    let from_local = local_boards.contains(&edge.from.path);
    let to_local = local_boards.contains(&edge.to.path);
    match (from_local, to_local) {
        (true, false) => Some(BridgeRole::Egress),
        (false, true) => Some(BridgeRole::Ingress),
        _ => None,
    }
}

enum EgressPort {
    Uart(UartSub),
    Analog(AnalogSub),
    Pulse(PulseSub),
    Scalar(ScalarSub),
}

enum IngressPort {
    Uart(UartPub),
    Analog(AnalogPub),
    Pulse(PulsePub),
    Scalar(ScalarPub),
}

struct EgressBridge {
    edge_id: HashedId<u64>,
    payload_kind: u8,
    port: EgressPort,
}

struct IngressBridge {
    payload_kind: u8,
    port: IngressPort,
}

/// Open iceoryx halves for every cross-host forwardable edge on this host.
pub struct EndpointBridge {
    egress: Vec<EgressBridge>,
    ingress: HashMap<u64, IngressBridge>,
}

impl EndpointBridge {
    pub fn open(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        topo: &LogicalTopology,
        local_boards: &[String],
    ) -> Result<Self, IpcError> {
        let local: HashSet<String> = local_boards.iter().cloned().collect();
        let mut egress = Vec::new();
        let mut ingress = HashMap::new();

        for edge in &topo.edges {
            let Some(role) = classify_edge(edge, &local) else {
                continue;
            };
            let id = edge_id(
                &edge.from.path,
                &edge.from.port,
                &edge.to.path,
                &edge.to.port,
            );
            let payload_kind = endpoint_payload_kind_wire(edge.payload).expect("classified");
            match role {
                BridgeRole::Egress => {
                    let port = open_egress(node, cluster_key, edge, id)?;
                    egress.push(EgressBridge {
                        edge_id: id,
                        payload_kind,
                        port,
                    });
                }
                BridgeRole::Ingress => {
                    let port = open_ingress(node, cluster_key, edge, id)?;
                    ingress.insert(id.get(), IngressBridge { payload_kind, port });
                }
            }
        }

        Ok(Self { egress, ingress })
    }

    /// Drain local TX iceoryx and multicast one Endpoint datagram per sample.
    pub fn pump_egress(
        &self,
        mcast: &ClusterMulticast,
        machine_id: MachineId,
    ) -> Result<(), IpcError> {
        for bridge in &self.egress {
            let samples = match drain_egress(&bridge.port, EGRESS_DRAIN_LIMIT) {
                Ok(samples) => samples,
                Err(err) => {
                    log::warn!(
                        "endpoint bridge egress drain edge_id={}: {err}",
                        bridge.edge_id
                    );
                    continue;
                }
            };
            for payload in samples {
                let msg = MulticastMessage::Endpoint {
                    machine_id,
                    edge_id: bridge.edge_id.get(),
                    payload_kind: bridge.payload_kind,
                    payload,
                };
                if let Err(err) = mcast.send(&msg) {
                    log::warn!(
                        "endpoint multicast send failed edge_id={}: {err}",
                        bridge.edge_id
                    );
                }
            }
        }
        Ok(())
    }

    /// Inject a remote Endpoint sample onto the matching local ingress publisher.
    pub fn handle_endpoint(&self, edge_id: u64, payload_kind: u8, payload: &[u8]) {
        let Some(bridge) = self.ingress.get(&edge_id) else {
            return;
        };
        if bridge.payload_kind != payload_kind {
            log::warn!(
                "endpoint ingress kind mismatch edge_id={edge_id:#x}: wire={payload_kind} expected={}",
                bridge.payload_kind
            );
            return;
        }
        if let Err(err) = publish_ingress(&bridge.port, payload_kind, payload) {
            log::warn!("endpoint ingress publish failed edge_id={edge_id:#x}: {err}");
        }
    }

    /// Drain egress samples and deliver them to `other` as if multicast arrived.
    ///
    /// Used by in-process tests (no UDP). Returns the number of samples forwarded.
    #[cfg(test)]
    pub fn forward_samples_to(&self, other: &Self) -> Result<usize, IpcError> {
        let mut n = 0;
        for bridge in &self.egress {
            let samples = drain_egress(&bridge.port, EGRESS_DRAIN_LIMIT)?;
            for payload in samples {
                other.handle_endpoint(bridge.edge_id.get(), bridge.payload_kind, &payload);
                n += 1;
            }
        }
        Ok(n)
    }

    #[must_use]
    pub fn egress_len(&self) -> usize {
        self.egress.len()
    }

    #[must_use]
    pub fn ingress_len(&self) -> usize {
        self.ingress.len()
    }
}

fn open_egress(
    node: &Node<ipc::Service>,
    cluster_key: &str,
    edge: &DirectedEdge,
    id: HashedId<u64>,
) -> Result<EgressPort, IpcError> {
    match edge.payload {
        PayloadKind::Uart => {
            let name = uart_edge(cluster_key, id);
            let capacity = LogicalTopology::uart_ring_len(edge).max(2) as usize;
            Ok(EgressPort::Uart(open_or_create_uart_subscriber(
                node, &name, capacity,
            )?))
        }
        PayloadKind::Analog => {
            let name = analog_edge(cluster_key, id);
            Ok(EgressPort::Analog(open_or_create_analog_subscriber(
                node,
                &name,
                DEFAULT_ANALOG_RING_LEN,
            )?))
        }
        PayloadKind::Pulse => {
            let name = pulse_edge(cluster_key, id);
            Ok(EgressPort::Pulse(open_or_create_pulse_subscriber(
                node,
                &name,
                DEFAULT_PULSE_RING_LEN,
            )?))
        }
        PayloadKind::Scalar => {
            let name = scalar_edge(cluster_key, id);
            Ok(EgressPort::Scalar(open_or_create_scalar_subscriber(
                node,
                &name,
                DEFAULT_SCALAR_RING_LEN,
            )?))
        }
        PayloadKind::Digital => Err(IpcError::Message(
            "endpoint bridge: Digital edges are not forwardable".into(),
        )),
    }
}

fn open_ingress(
    node: &Node<ipc::Service>,
    cluster_key: &str,
    edge: &DirectedEdge,
    id: HashedId<u64>,
) -> Result<IngressPort, IpcError> {
    match edge.payload {
        PayloadKind::Uart => {
            let name = uart_edge(cluster_key, id);
            let capacity = LogicalTopology::uart_ring_len(edge).max(2) as usize;
            Ok(IngressPort::Uart(open_or_create_uart_publisher(
                node, &name, capacity,
            )?))
        }
        PayloadKind::Analog => {
            let name = analog_edge(cluster_key, id);
            Ok(IngressPort::Analog(open_or_create_analog_publisher(
                node,
                &name,
                DEFAULT_ANALOG_RING_LEN,
            )?))
        }
        PayloadKind::Pulse => {
            let name = pulse_edge(cluster_key, id);
            Ok(IngressPort::Pulse(open_or_create_pulse_publisher(
                node,
                &name,
                DEFAULT_PULSE_RING_LEN,
            )?))
        }
        PayloadKind::Scalar => {
            let name = scalar_edge(cluster_key, id);
            Ok(IngressPort::Scalar(open_or_create_scalar_publisher(
                node,
                &name,
                DEFAULT_SCALAR_RING_LEN,
            )?))
        }
        PayloadKind::Digital => Err(IpcError::Message(
            "endpoint bridge: Digital edges are not forwardable".into(),
        )),
    }
}

fn drain_egress(port: &EgressPort, limit: usize) -> Result<Vec<Vec<u8>>, IpcError> {
    let mut out = Vec::new();
    match port {
        EgressPort::Uart(sub) => {
            for _ in 0..limit {
                match sub
                    .receive()
                    .map_err(|e| IpcError::Message(format!("bridge uart recv: {e:?}")))?
                {
                    Some(sample) => out.push(encode_uart_payload(&*sample)),
                    None => break,
                }
            }
        }
        EgressPort::Analog(sub) => {
            for _ in 0..limit {
                match sub
                    .receive()
                    .map_err(|e| IpcError::Message(format!("bridge analog recv: {e:?}")))?
                {
                    Some(sample) => out.push(encode_analog_payload(&*sample)),
                    None => break,
                }
            }
        }
        EgressPort::Pulse(sub) => {
            for _ in 0..limit {
                match sub
                    .receive()
                    .map_err(|e| IpcError::Message(format!("bridge pulse recv: {e:?}")))?
                {
                    Some(sample) => out.push(encode_pulse_payload(&*sample)),
                    None => break,
                }
            }
        }
        EgressPort::Scalar(sub) => {
            for _ in 0..limit {
                match sub
                    .receive()
                    .map_err(|e| IpcError::Message(format!("bridge scalar recv: {e:?}")))?
                {
                    Some(sample) => out.push(encode_scalar_payload(&*sample)),
                    None => break,
                }
            }
        }
    }
    Ok(out)
}

fn publish_ingress(port: &IngressPort, payload_kind: u8, payload: &[u8]) -> Result<(), IpcError> {
    use crate::cluster_mcast::{
        ENDPOINT_KIND_ANALOG, ENDPOINT_KIND_PULSE, ENDPOINT_KIND_SCALAR, ENDPOINT_KIND_UART,
    };
    match (port, payload_kind) {
        (IngressPort::Uart(puber), ENDPOINT_KIND_UART) => {
            let frame = decode_uart_payload(payload)
                .ok_or_else(|| IpcError::Message("bridge uart: bad payload".into()))?;
            puber
                .send_copy(frame)
                .map_err(|e| IpcError::Message(format!("bridge uart send: {e:?}")))?;
        }
        (IngressPort::Analog(puber), ENDPOINT_KIND_ANALOG) => {
            let v = decode_analog_payload(payload)
                .ok_or_else(|| IpcError::Message("bridge analog: bad payload".into()))?;
            puber
                .send_copy(v)
                .map_err(|e| IpcError::Message(format!("bridge analog send: {e:?}")))?;
        }
        (IngressPort::Pulse(puber), ENDPOINT_KIND_PULSE) => {
            let v = decode_pulse_payload(payload)
                .ok_or_else(|| IpcError::Message("bridge pulse: bad payload".into()))?;
            puber
                .send_copy(v)
                .map_err(|e| IpcError::Message(format!("bridge pulse send: {e:?}")))?;
        }
        (IngressPort::Scalar(puber), ENDPOINT_KIND_SCALAR) => {
            let v = decode_scalar_payload(payload)
                .ok_or_else(|| IpcError::Message("bridge scalar: bad payload".into()))?;
            puber
                .send_copy(v)
                .map_err(|e| IpcError::Message(format!("bridge scalar send: {e:?}")))?;
        }
        _ => {
            return Err(IpcError::Message(
                "bridge ingress: payload_kind does not match port".into(),
            ));
        }
    }
    Ok(())
}

/// Ignore Endpoint datagrams that originated on this machine (defensive; loop
/// is already disabled on the socket).
#[must_use]
pub fn should_ignore_endpoint(machine_id: MachineId, local: MachineId) -> bool {
    machine_id == local
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_mcast::{
        ENDPOINT_KIND_ANALOG, ENDPOINT_KIND_PULSE, ENDPOINT_KIND_SCALAR, ENDPOINT_KIND_UART,
    };
    use crate::ipc::{NodePulsePorts, create_node, isolated_config, new_cluster_key};
    use crate::topology::{EndpointDirection, EndpointSpec, PortRef};
    use sim_kernel::Pulse;
    use std::time::Duration;

    fn edge(from: &str, to: &str, payload: PayloadKind) -> DirectedEdge {
        DirectedEdge {
            from: PortRef {
                path: from.into(),
                port: "p".into(),
            },
            to: PortRef {
                path: to.into(),
                port: "p".into(),
            },
            route: vec![],
            payload,
            uart_ring_len: None,
        }
    }

    #[test]
    fn classify_local_remote_roles() {
        let local = HashSet::from(["a".into()]);
        assert_eq!(
            classify_edge(&edge("a", "b", PayloadKind::Uart), &local),
            Some(BridgeRole::Egress)
        );
        assert_eq!(
            classify_edge(&edge("b", "a", PayloadKind::Pulse), &local),
            Some(BridgeRole::Ingress)
        );
        assert_eq!(
            classify_edge(&edge("a", "a", PayloadKind::Uart), &local),
            None
        );
        assert_eq!(
            classify_edge(&edge("b", "c", PayloadKind::Uart), &local),
            None
        );
        assert_eq!(
            classify_edge(&edge("a", "b", PayloadKind::Digital), &local),
            None
        );
    }

    #[test]
    fn wire_kind_constants_match_helpers() {
        assert_eq!(
            endpoint_payload_kind_wire(PayloadKind::Uart),
            Some(ENDPOINT_KIND_UART)
        );
        assert_eq!(
            endpoint_payload_kind_wire(PayloadKind::Analog),
            Some(ENDPOINT_KIND_ANALOG)
        );
        assert_eq!(
            endpoint_payload_kind_wire(PayloadKind::Pulse),
            Some(ENDPOINT_KIND_PULSE)
        );
        assert_eq!(
            endpoint_payload_kind_wire(PayloadKind::Scalar),
            Some(ENDPOINT_KIND_SCALAR)
        );
        assert_eq!(endpoint_payload_kind_wire(PayloadKind::Digital), None);
    }

    #[test]
    fn pulse_bridge_tx_egress_ingress_rx_round_trip() {
        // Two iceoryx roots = two hosts. Same-root would fight max_publishers(1)
        // between TX board and ingress arbiter.
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let cfg_a = isolated_config(&dir_a.path().join("iox")).unwrap();
        let cfg_b = isolated_config(&dir_b.path().join("iox")).unwrap();
        let topo = LogicalTopology {
            boards: vec![
                crate::topology::BoardSpec {
                    id: "a".into(),
                    kind: "plant".into(),
                    elf: None,
                    endpoints: vec![EndpointSpec {
                        name: "p".into(),
                        direction: EndpointDirection::Out,
                        payload: PayloadKind::Pulse,
                    }],
                },
                crate::topology::BoardSpec {
                    id: "b".into(),
                    kind: "rl78".into(),
                    elf: None,
                    endpoints: vec![EndpointSpec {
                        name: "p".into(),
                        direction: EndpointDirection::In,
                        payload: PayloadKind::Pulse,
                    }],
                },
            ],
            edges: vec![DirectedEdge {
                from: PortRef {
                    path: "a".into(),
                    port: "p".into(),
                },
                to: PortRef {
                    path: "b".into(),
                    port: "p".into(),
                },
                route: vec![],
                payload: PayloadKind::Pulse,
                uart_ring_len: None,
            }],
            margin_ns: 1,
            booting_timeout_ms: 1000,
        };

        let n_tx = create_node(&cfg_a, "bridge-tx").unwrap();
        let n_egress = create_node(&cfg_a, "bridge-egress").unwrap();
        let n_ingress = create_node(&cfg_b, "bridge-ingress").unwrap();
        let n_rx = create_node(&cfg_b, "bridge-rx").unwrap();

        let tx = NodePulsePorts::open_for_board(&n_tx, &key, "a", &topo.edges).unwrap();
        let egress = EndpointBridge::open(&n_egress, &key, &topo, &["a".into()]).unwrap();
        let ingress = EndpointBridge::open(&n_ingress, &key, &topo, &["b".into()]).unwrap();
        let rx = NodePulsePorts::open_for_board(&n_rx, &key, "b", &topo.edges).unwrap();

        assert_eq!(egress.egress_len(), 1);
        assert_eq!(egress.ingress_len(), 0);
        assert_eq!(ingress.egress_len(), 0);
        assert_eq!(ingress.ingress_len(), 1);

        let id = edge_id("a", "p", "b", "p");
        let sample = Pulse {
            hi_width_ns: 111,
            lo_width_ns: 222,
        };
        tx.producers.get(&id).unwrap().send_copy(sample).unwrap();

        let mut forwarded = 0usize;
        for _ in 0..100 {
            forwarded += egress.forward_samples_to(&ingress).unwrap();
            if forwarded > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(forwarded, 1);

        let mut got = None;
        for _ in 0..100 {
            if let Ok(Some(s)) = rx.consumers.get(&id).unwrap().receive() {
                got = Some(*s);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, Some(sample));
    }
}
