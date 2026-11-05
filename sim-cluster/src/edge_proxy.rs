//! Arbiter proxy for cross-machine UART / analog edges.
//!
//! Path: `Node → iceoryx2 → local arbiter → data UDP (sliding window) → peer
//! arbiter → iceoryx2 → Node`.
//!
//! Every edge is classified from this arbiter's point of view (see
//! [`plan_cross_edges`]):
//!
//! * both boards local → **direct**: the two Nodes talk over iceoryx2 and the
//!   arbiter does not touch the edge (it must not open a port, because the
//!   services are SPSC);
//! * exactly one board local → **cross-machine**: the arbiter opens the end of
//!   the iceoryx2 service that the local Node does not hold and pumps samples
//!   to / from the peer arbiter that owns the remote board;
//! * no board local → somebody else's business.
//!
//! Digital edges carry no data between nodes yet (the data plane skips them), so
//! they are not proxied either.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use iceoryx2::prelude::*;

use crate::arbiter::ArbiterError;
use crate::cluster_net::{DataLink, EdgeMessage, EdgeValue};
use crate::ipc::{ProxyAnalogPorts, ProxyUartPorts, edge_id};
use crate::orchestrate::{AssignmentTable, BackendKey};
use crate::topology::{DirectedEdge, PayloadKind};

/// Samples moved from one iceoryx2 service into the data link per pump call.
const EGRESS_BURST: usize = 256;

/// One cross-machine edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossEdge {
    /// `ipc::edge_id` of the edge.
    pub id: String,
    pub edge: DirectedEdge,
    /// Peer arbiter that owns the remote board.
    pub peer: BackendKey,
}

/// Cross-machine edges of one arbiter, split by direction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EdgeProxyPlan {
    /// Local board is `from`: the arbiter subscribes and ships samples to `peer`.
    pub egress: Vec<CrossEdge>,
    /// Local board is `to`: samples from `peer` are published to the local board.
    pub ingress: Vec<CrossEdge>,
}

impl EdgeProxyPlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.egress.is_empty() && self.ingress.is_empty()
    }
}

/// Classify `edges` for an arbiter that runs `local_boards`.
///
/// `assignment` is the full board → backend placement; it names the peer that
/// owns the remote board of each cross edge. Edges between two local boards are
/// left out on purpose.
pub fn plan_cross_edges(
    edges: &[DirectedEdge],
    local_boards: &[String],
    assignment: &AssignmentTable,
) -> Result<EdgeProxyPlan, ArbiterError> {
    let local: HashSet<&str> = local_boards.iter().map(String::as_str).collect();
    let mut plan = EdgeProxyPlan::default();
    for edge in edges {
        if !matches!(edge.payload, PayloadKind::Uart | PayloadKind::Analog) {
            continue;
        }
        let from_local = local.contains(edge.from_board.as_str());
        let to_local = local.contains(edge.to_board.as_str());
        let (remote_board, egress) = match (from_local, to_local) {
            (true, true) | (false, false) => continue,
            (true, false) => (edge.to_board.as_str(), true),
            (false, true) => (edge.from_board.as_str(), false),
        };
        let Some(peer) = assignment.backend_for_board(remote_board) else {
            return Err(ArbiterError::Message(format!(
                "edge {}:{} -> {}:{}: board `{remote_board}` has no backend in the assignment",
                edge.from_board, edge.from_endpoint, edge.to_board, edge.to_endpoint
            )));
        };
        let cross = CrossEdge {
            id: edge_id(
                &edge.from_board,
                &edge.from_endpoint,
                &edge.to_board,
                &edge.to_endpoint,
            ),
            edge: edge.clone(),
            peer: peer.to_owned(),
        };
        if egress {
            plan.egress.push(cross);
        } else {
            plan.ingress.push(cross);
        }
    }
    Ok(plan)
}

/// Opened iceoryx2 ends plus the data link for this arbiter's cross edges.
pub struct EdgeProxy {
    link: DataLink,
    uart: ProxyUartPorts,
    analog: ProxyAnalogPorts,
    egress: Vec<CrossEdge>,
    /// `edge_id` → (peer allowed to deliver it, payload kind).
    ingress: HashMap<String, (BackendKey, PayloadKind)>,
    rejected: u64,
}

impl EdgeProxy {
    /// Open the arbiter-side iceoryx2 ends of every edge in `plan`.
    pub fn open(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        plan: EdgeProxyPlan,
        link: DataLink,
    ) -> Result<Self, ArbiterError> {
        let egress_edges: Vec<&DirectedEdge> = plan.egress.iter().map(|c| &c.edge).collect();
        let ingress_edges: Vec<&DirectedEdge> = plan.ingress.iter().map(|c| &c.edge).collect();
        let uart = ProxyUartPorts::open(node, cluster_key, &egress_edges, &ingress_edges)?;
        let analog = ProxyAnalogPorts::open(node, cluster_key, &egress_edges, &ingress_edges)?;
        let ingress = plan
            .ingress
            .iter()
            .map(|c| (c.id.clone(), (c.peer.clone(), c.edge.payload)))
            .collect();
        Ok(Self {
            link,
            uart,
            analog,
            egress: plan.egress,
            ingress,
            rejected: 0,
        })
    }

    /// Samples dropped because they named an unknown edge, came from the wrong
    /// peer, or did not match the edge's payload kind.
    #[must_use]
    pub fn rejected(&self) -> u64 {
        self.rejected
    }

    /// One pump round: iceoryx2 → UDP, UDP housekeeping, UDP → iceoryx2.
    pub fn pump(&mut self, now: Instant) -> Result<(), ArbiterError> {
        for cross in &self.egress {
            for _ in 0..EGRESS_BURST {
                // Stop draining while the peer's backlog is full: the iceoryx2
                // buffer (not an unbounded queue) absorbs the pressure.
                if !self.link.can_send(&cross.peer) {
                    break;
                }
                let value = match cross.edge.payload {
                    PayloadKind::Uart => self.uart.receive(&cross.id)?.map(EdgeValue::Uart),
                    PayloadKind::Analog => self.analog.receive(&cross.id)?.map(EdgeValue::Analog),
                    PayloadKind::Digital => None,
                };
                let Some(value) = value else {
                    break;
                };
                let message = EdgeMessage {
                    edge_id: cross.id.clone(),
                    value,
                };
                self.link
                    .send(&cross.peer, message.encode())
                    .map_err(|err| ArbiterError::Message(err.to_string()))?;
            }
        }
        for (peer, bytes) in self.link.poll(now) {
            let Some(message) = EdgeMessage::decode(&bytes) else {
                self.rejected += 1;
                log::warn!("edge proxy: undecodable message from {peer}");
                continue;
            };
            let allowed = self.ingress.get(&message.edge_id);
            let result = match (allowed, message.value) {
                (Some((from, PayloadKind::Uart)), EdgeValue::Uart(frame)) if *from == peer => {
                    self.uart.publish(&message.edge_id, frame)
                }
                (Some((from, PayloadKind::Analog)), EdgeValue::Analog(value)) if *from == peer => {
                    self.analog.publish(&message.edge_id, value)
                }
                _ => {
                    self.rejected += 1;
                    log::warn!(
                        "edge proxy: dropped sample for edge `{}` from {peer}",
                        message.edge_id
                    );
                    continue;
                }
            };
            if let Err(err) = result {
                log::warn!("edge proxy: {err}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
    use std::time::Duration;

    use sim_kernel::{AnalogVoltage, UartFrame};

    use crate::ipc::{
        NodeAnalogPorts, NodeUartPorts, create_node, isolated_config, new_cluster_key,
    };
    use crate::orchestrate::{ArbiterEndpoints, BoardAssignment};

    const M1: &str = "10.0.0.1:8091";
    const M2: &str = "10.0.0.2:8091";

    fn edge(from: &str, fep: &str, to: &str, tep: &str, payload: PayloadKind) -> DirectedEdge {
        DirectedEdge {
            from_board: from.into(),
            from_endpoint: fep.into(),
            to_board: to.into(),
            to_endpoint: tep.into(),
            payload,
            uart_ring_len: matches!(payload, PayloadKind::Uart).then_some(8),
        }
    }

    fn assignment(entries: &[(&str, &str)]) -> AssignmentTable {
        AssignmentTable {
            entries: entries
                .iter()
                .map(|(board, backend)| BoardAssignment {
                    board_id: (*board).into(),
                    backend: (*backend).into(),
                })
                .collect(),
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    // a, c on M1; b on M2.
    fn topo_edges() -> Vec<DirectedEdge> {
        vec![
            edge("a", "tx", "b", "rx", PayloadKind::Uart), // cross, egress on M1
            edge("b", "tx", "a", "rx", PayloadKind::Uart), // cross, ingress on M1
            edge("a", "dac", "b", "vin", PayloadKind::Analog), // cross, egress on M1
            edge("a", "tx2", "c", "rx", PayloadKind::Uart), // same host
            edge("c", "dac", "a", "vin", PayloadKind::Analog), // same host
            edge("a", "d", "b", "d", PayloadKind::Digital), // never proxied
        ]
    }

    fn placement() -> AssignmentTable {
        assignment(&[("a", M1), ("c", M1), ("b", M2)])
    }

    #[test]
    fn same_host_edges_are_not_proxied() {
        let plan = plan_cross_edges(&topo_edges(), &ids(&["a", "c"]), &placement()).unwrap();
        let egress: Vec<&str> = plan.egress.iter().map(|c| c.id.as_str()).collect();
        let ingress: Vec<&str> = plan.ingress.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(egress, vec!["a:tx->b:rx", "a:dac->b:vin"]);
        assert_eq!(ingress, vec!["b:tx->a:rx"]);
        assert!(plan.egress.iter().all(|c| c.peer == M2));
        assert!(plan.ingress.iter().all(|c| c.peer == M2));
        for cross in plan.egress.iter().chain(&plan.ingress) {
            assert!(
                !(cross.edge.from_board == "c" || cross.edge.to_board == "c"),
                "{} touches only local boards on one side",
                cross.id
            );
        }
    }

    #[test]
    fn other_side_sees_the_mirrored_plan() {
        let plan = plan_cross_edges(&topo_edges(), &ids(&["b"]), &placement()).unwrap();
        let ingress: Vec<&str> = plan.ingress.iter().map(|c| c.id.as_str()).collect();
        let egress: Vec<&str> = plan.egress.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ingress, vec!["a:tx->b:rx", "a:dac->b:vin"]);
        assert_eq!(egress, vec!["b:tx->a:rx"]);
        assert!(plan.ingress.iter().all(|c| c.peer == M1));
    }

    #[test]
    fn everything_local_means_empty_plan() {
        let all = ids(&["a", "b", "c"]);
        let one_backend = assignment(&[("a", M1), ("b", M1), ("c", M1)]);
        assert!(
            plan_cross_edges(&topo_edges(), &all, &one_backend)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn remote_board_without_placement_is_an_error() {
        let partial = assignment(&[("a", M1), ("c", M1)]);
        assert!(plan_cross_edges(&topo_edges(), &ids(&["a", "c"]), &partial).is_err());
    }

    fn loopback() -> UdpSocket {
        let s = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        s.set_nonblocking(true).unwrap();
        s
    }

    fn endpoint(backend: &str, socket: &UdpSocket) -> ArbiterEndpoints {
        ArbiterEndpoints {
            backend: backend.into(),
            advertise_ip: "127.0.0.1".into(),
            control_tcp_port: 1,
            data_udp_port: socket.local_addr().unwrap().port(),
        }
    }

    /// Two "machines" (separate iceoryx2 roots) in one process.
    #[test]
    fn cross_machine_uart_and_analog_reach_the_peer_node_and_same_host_stays_direct() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let cfg1 = isolated_config(&dir1.path().join("iox")).unwrap();
        let cfg2 = isolated_config(&dir2.path().join("iox")).unwrap();
        let key1 = new_cluster_key();
        let key2 = new_cluster_key();
        let edges = topo_edges();

        let s1 = loopback();
        let s2 = loopback();
        let e1 = endpoint(M1, &s1);
        let e2 = endpoint(M2, &s2);

        // Machine 1: arbiter + Nodes a and c.
        let arb1 = create_node(&cfg1, "arb1").unwrap();
        let plan1 = plan_cross_edges(&edges, &ids(&["a", "c"]), &placement()).unwrap();
        let link1 = DataLink::new(s1, "sim", M1, std::slice::from_ref(&e2), 32).unwrap();
        let mut proxy1 = EdgeProxy::open(&arb1, &key1, plan1, link1).unwrap();
        let na = create_node(&cfg1, "node-a").unwrap();
        let nc = create_node(&cfg1, "node-c").unwrap();
        // Nodes open after the arbiter: succeeds only if the arbiter did not
        // take the Node-held end of any edge, in particular a<->c.
        let a_uart = NodeUartPorts::open_for_board(&na, &key1, "a", &edges).unwrap();
        let a_analog = NodeAnalogPorts::open_for_board(&na, &key1, "a", &edges).unwrap();
        let c_uart = NodeUartPorts::open_for_board(&nc, &key1, "c", &edges).unwrap();
        let c_analog = NodeAnalogPorts::open_for_board(&nc, &key1, "c", &edges).unwrap();

        // Machine 2: arbiter + Node b.
        let arb2 = create_node(&cfg2, "arb2").unwrap();
        let plan2 = plan_cross_edges(&edges, &ids(&["b"]), &placement()).unwrap();
        let link2 = DataLink::new(s2, "sim", M2, &[e1], 32).unwrap();
        let mut proxy2 = EdgeProxy::open(&arb2, &key2, plan2, link2).unwrap();
        let nb = create_node(&cfg2, "node-b").unwrap();
        let b_uart = NodeUartPorts::open_for_board(&nb, &key2, "b", &edges).unwrap();
        let b_analog = NodeAnalogPorts::open_for_board(&nb, &key2, "b", &edges).unwrap();

        let frame = |n: u16| UartFrame {
            data: n,
            ..UartFrame::default()
        };
        // a -> b over UART (burst larger than the 8-deep ring is avoided on purpose).
        for n in 0..6u16 {
            a_uart.try_send("a:tx->b:rx", frame(n)).unwrap();
        }
        // a -> b analog.
        a_analog.producers["a:dac->b:vin"]
            .send_copy(AnalogVoltage(1_234_000))
            .unwrap();
        // b -> a over UART.
        b_uart.try_send("b:tx->a:rx", frame(0x55)).unwrap();
        // a -> c stays on machine 1, node to node.
        a_uart.try_send("a:tx2->c:rx", frame(0x77)).unwrap();
        c_analog.producers["c:dac->a:vin"]
            .send_copy(AnalogVoltage(-9))
            .unwrap();

        // The same-host edges arrive without any proxy round.
        let mut direct_uart = None;
        let mut direct_analog = None;
        for _ in 0..200 {
            if direct_uart.is_none() {
                direct_uart = c_uart.drain_all(4).unwrap().into_iter().next();
            }
            if direct_analog.is_none() {
                direct_analog = a_analog.consumers["c:dac->a:vin"]
                    .receive()
                    .unwrap()
                    .map(|s| *s);
            }
            if direct_uart.is_some() && direct_analog.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(direct_uart.unwrap().1, vec![frame(0x77)]);
        assert_eq!(direct_analog, Some(AnalogVoltage(-9)));

        let mut got_uart: Vec<UartFrame> = Vec::new();
        let mut got_analog: Option<AnalogVoltage> = None;
        let mut got_back: Vec<UartFrame> = Vec::new();
        for _ in 0..400 {
            proxy1.pump(Instant::now()).unwrap();
            proxy2.pump(Instant::now()).unwrap();
            for (_, frames) in b_uart.drain_all(16).unwrap() {
                got_uart.extend(frames);
            }
            if let Some(s) = b_analog.consumers["a:dac->b:vin"].receive().unwrap() {
                got_analog = Some(*s);
            }
            for (_, frames) in a_uart.drain_all(16).unwrap() {
                got_back.extend(frames);
            }
            if got_uart.len() == 6 && got_analog.is_some() && got_back.len() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(got_uart, (0..6u16).map(frame).collect::<Vec<_>>());
        assert_eq!(got_analog, Some(AnalogVoltage(1_234_000)));
        assert_eq!(got_back, vec![frame(0x55)]);
        assert_eq!(proxy1.rejected() + proxy2.rejected(), 0);
    }
}
