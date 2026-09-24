//! Board data-plane pumping: topology edges ↔ [`BoardPorts`].
//!
//! Control IPC stays in [`crate::board_runtime`]; this module owns payload
//! fan-in/fan-out. New payload kinds add a map + plane pair and a branch in
//! [`open_data_planes`] — no inventory / registration macros.
//!
//! [`open`](UartDataPlaneMap::open) builds edge↔endpoint mapping (and opens
//! iceoryx ports). [`bind`](UartDataPlaneMap::bind) consumes that map with
//! [`BoardPorts`] and yields a [`UartDataPlane`] whose [`pump_rx`] /
//! [`pump_tx`] use pre-wired lanes with no name lookup.

use std::collections::{HashMap, HashSet};

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::Node;
use iceoryx2::prelude::ipc;
use sim_kernel::{BoardPorts, OutPort, SourcePort, UartFrame};

use crate::ipc::{IpcError, NodeUartPorts, edge_id};
use crate::topology::{BoardSpec, DirectedEdge, EndpointDirection, LogicalTopology, PayloadKind};

type UartPub = Publisher<ipc::Service, UartFrame, ()>;
type UartSub = Subscriber<ipc::Service, UartFrame, ()>;

/// Failures from data-plane open / bind / pump.
#[derive(Debug, thiserror::Error)]
pub enum DataPlaneError {
    #[error("ipc error: {0}")]
    Ipc(#[from] IpcError),
    #[error("{0}")]
    Message(String),
}

/// Opened iceoryx / topology mapping; consumed by [`DataPlaneMap::bind`].
pub trait DataPlaneMap {
    fn bind(self: Box<Self>, ports: &BoardPorts) -> Result<Box<dyn DataPlane>, DataPlaneError>;
}

/// Bound transport between iceoryx2 and board-edge ports (pump only).
pub trait DataPlane {
    fn pump_rx(&mut self) -> Result<(), DataPlaneError>;
    fn pump_tx(&mut self) -> Result<(), DataPlaneError>;
}

struct UartRxLane {
    sub: UartSub,
    port: SourcePort<UartFrame>,
}

struct UartTxLane {
    port: OutPort<UartFrame>,
    pubs: Vec<UartPub>,
}

struct PendingRx {
    edge_id: String,
    ep: String,
    sub: UartSub,
}

struct PendingTx {
    ep: String,
    edge_ids: Vec<String>,
}

/// UART edge↔endpoint mapping opened before board ports exist.
pub struct UartDataPlaneMap {
    producers: HashMap<String, UartPub>,
    pending_rx: Vec<PendingRx>,
    pending_tx: Vec<PendingTx>,
    required_ins: HashSet<String>,
    required_outs: HashSet<String>,
}

impl UartDataPlaneMap {
    pub fn open(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_id: &str,
        board: &BoardSpec,
        topo_edges: &[DirectedEdge],
    ) -> Result<Self, IpcError> {
        let NodeUartPorts {
            mut producers,
            mut consumers,
        } = NodeUartPorts::open_for_board(node, cluster_key, board_id, topo_edges)?;

        let mut pending_rx = Vec::new();
        let mut tx_ep_to_edges: HashMap<String, Vec<String>> = HashMap::new();

        for edge in topo_edges {
            if edge.payload != PayloadKind::Uart {
                continue;
            }
            let id = edge_id(
                &edge.from_board,
                &edge.from_endpoint,
                &edge.to_board,
                &edge.to_endpoint,
            );
            match (
                edge.to_board.as_str() == board_id,
                edge.from_board.as_str() == board_id,
            ) {
                (true, _) => {
                    let Some(sub) = consumers.remove(&id) else {
                        return Err(IpcError::Message(format!("missing uart consumer for {id}")));
                    };
                    pending_rx.push(PendingRx {
                        edge_id: id,
                        ep: edge.to_endpoint.clone(),
                        sub,
                    });
                }
                (false, true) => {
                    tx_ep_to_edges
                        .entry(edge.from_endpoint.clone())
                        .or_default()
                        .push(id);
                }
                (false, false) => {}
            }
        }

        let pending_tx: Vec<PendingTx> = tx_ep_to_edges
            .into_iter()
            .map(|(ep, edge_ids)| PendingTx { ep, edge_ids })
            .collect();

        // Keep only producers still needed for pending TX edges.
        let needed: HashSet<&str> = pending_tx
            .iter()
            .flat_map(|t| t.edge_ids.iter().map(String::as_str))
            .collect();
        producers.retain(|id, _| needed.contains(id.as_str()));

        let mut required_ins = HashSet::new();
        let mut required_outs = HashSet::new();
        for ep in &board.endpoints {
            if ep.payload != PayloadKind::Uart {
                continue;
            }
            match ep.direction {
                EndpointDirection::In => {
                    required_ins.insert(ep.name.clone());
                }
                EndpointDirection::Out => {
                    required_outs.insert(ep.name.clone());
                }
            }
        }

        Ok(Self {
            producers,
            pending_rx,
            pending_tx,
            required_ins,
            required_outs,
        })
    }

    pub fn bind(mut self, ports: &BoardPorts) -> Result<UartDataPlane, DataPlaneError> {
        for name in &self.required_ins {
            if ports.in_port::<UartFrame>(name).is_none() {
                return Err(DataPlaneError::Message(format!(
                    "missing InPort<UartFrame> for topology endpoint `{name}`"
                )));
            }
        }
        for name in &self.required_outs {
            if ports.out_port::<UartFrame>(name).is_none() {
                return Err(DataPlaneError::Message(format!(
                    "missing OutPort<UartFrame> for topology endpoint `{name}`"
                )));
            }
        }

        let mut rx = Vec::with_capacity(self.pending_rx.len());
        for pending in self.pending_rx {
            let port = ports
                .in_port::<UartFrame>(&pending.ep)
                .map(|p| p.port().clone())
                .ok_or_else(|| {
                    DataPlaneError::Message(format!(
                        "uart rx: no InPort for endpoint `{}` (edge {})",
                        pending.ep, pending.edge_id
                    ))
                })?;
            rx.push(UartRxLane {
                sub: pending.sub,
                port,
            });
        }

        let mut tx = Vec::with_capacity(self.pending_tx.len());
        for pending in self.pending_tx {
            let port = ports
                .out_port::<UartFrame>(&pending.ep)
                .cloned()
                .ok_or_else(|| {
                    DataPlaneError::Message(format!(
                        "uart tx: no OutPort for endpoint `{}`",
                        pending.ep
                    ))
                })?;
            let mut pubs = Vec::with_capacity(pending.edge_ids.len());
            for edge_id in pending.edge_ids {
                let puber = self.producers.remove(&edge_id).ok_or_else(|| {
                    DataPlaneError::Message(format!("missing uart producer for {edge_id}"))
                })?;
                pubs.push(puber);
            }
            tx.push(UartTxLane { port, pubs });
        }

        Ok(UartDataPlane { rx, tx })
    }
}

impl DataPlaneMap for UartDataPlaneMap {
    fn bind(self: Box<Self>, ports: &BoardPorts) -> Result<Box<dyn DataPlane>, DataPlaneError> {
        Ok(Box::new((*self).bind(ports)?))
    }
}

/// Bound UART data plane for one board (lanes only).
pub struct UartDataPlane {
    rx: Vec<UartRxLane>,
    tx: Vec<UartTxLane>,
}

impl DataPlane for UartDataPlane {
    fn pump_rx(&mut self) -> Result<(), DataPlaneError> {
        for lane in &self.rx {
            for _ in 0..64 {
                match lane
                    .sub
                    .receive()
                    .map_err(|e| DataPlaneError::Message(format!("uart recv: {e:?}")))?
                {
                    Some(sample) => lane.port.drive(*sample),
                    None => break,
                }
            }
        }
        Ok(())
    }

    fn pump_tx(&mut self) -> Result<(), DataPlaneError> {
        for lane in &self.tx {
            for frame in lane.port.drain_pending() {
                for puber in &lane.pubs {
                    if let Err(err) = puber.send_copy(frame) {
                        // Drop the frame: SHM pool exhaustion is rare with a
                        // large enough ring; do not stall later frames.
                        eprintln!("uart tx try_send: {err:?}");
                    }
                }
            }
        }
        Ok(())
    }
}

/// Open every data-plane map needed by `board_id` for the given topology.
pub fn open_data_planes(
    node: &Node<ipc::Service>,
    cluster_key: &str,
    board_id: &str,
    board: &BoardSpec,
    topo: &LogicalTopology,
) -> Result<Vec<Box<dyn DataPlaneMap>>, DataPlaneError> {
    let mut kinds = HashSet::new();
    for edge in &topo.edges {
        if edge.from_board == board_id || edge.to_board == board_id {
            kinds.insert(edge.payload);
        }
    }
    for ep in &board.endpoints {
        kinds.insert(ep.payload);
    }

    let mut maps: Vec<Box<dyn DataPlaneMap>> = Vec::new();
    for kind in kinds {
        match kind {
            PayloadKind::Uart => {
                let map = UartDataPlaneMap::open(node, cluster_key, board_id, board, &topo.edges)?;
                maps.push(Box::new(map));
            }
            PayloadKind::Digital => {
                // Out of scope for issue #62.
            }
        }
    }
    Ok(maps)
}

/// Bind opened maps against board ports into pumpable planes.
pub fn bind_data_planes(
    maps: Vec<Box<dyn DataPlaneMap>>,
    ports: &BoardPorts,
) -> Result<Vec<Box<dyn DataPlane>>, DataPlaneError> {
    maps.into_iter().map(|m| m.bind(ports)).collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sim_kernel::{BoardPorts, InPort, OutPort, UartFrame, Wire};

    use super::*;
    use crate::ipc::{create_node, isolated_config, new_cluster_key};
    use crate::topology::{EndpointDirection, EndpointSpec};

    fn uart_board(id: &str) -> BoardSpec {
        BoardSpec {
            id: id.into(),
            kind: "rl78".into(),
            elf: None,
            endpoints: vec![
                EndpointSpec {
                    name: "uart0_tx".into(),
                    direction: EndpointDirection::Out,
                    payload: PayloadKind::Uart,
                },
                EndpointSpec {
                    name: "uart0_rx".into(),
                    direction: EndpointDirection::In,
                    payload: PayloadKind::Uart,
                },
            ],
        }
    }

    fn two_board_topo() -> LogicalTopology {
        LogicalTopology {
            boards: vec![uart_board("a"), uart_board("b")],
            edges: vec![
                DirectedEdge {
                    from_board: "a".into(),
                    from_endpoint: "uart0_tx".into(),
                    to_board: "b".into(),
                    to_endpoint: "uart0_rx".into(),
                    payload: PayloadKind::Uart,
                    uart_ring_len: Some(8),
                },
                DirectedEdge {
                    from_board: "b".into(),
                    from_endpoint: "uart0_tx".into(),
                    to_board: "a".into(),
                    to_endpoint: "uart0_rx".into(),
                    payload: PayloadKind::Uart,
                    uart_ring_len: Some(8),
                },
            ],
            margin_ns: 1_000,
            headroom_threshold_ns: 500,
            ready_timeout_ms: 1_000,
        }
    }

    fn board_ports_with_rx_tap(tap_name: &str) -> (BoardPorts, OutPort<UartFrame>) {
        let mut ports = BoardPorts::new();
        ports
            .insert_out(OutPort::<UartFrame>::new("uart0_tx").unwrap())
            .unwrap();
        let mut rx = InPort::<UartFrame>::new("uart0_rx").unwrap();
        let tap = OutPort::<UartFrame>::new(tap_name).unwrap();
        let _wire = Wire::new().source(rx.port_mut()).sink({
            let tap = tap.clone();
            move |values, changed| tap.on_input(values, changed)
        });
        ports.insert_in(rx).unwrap();
        (ports, tap)
    }

    #[test]
    fn uart_dataplane_tx_ipc_rx_round_trip() {
        // TX on board A, then drop A and RX on board B. IPC retains the sample
        // across the hand-off (names are per-BoardPorts, so both boards may use
        // uart0_*).
        let dir = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let config = isolated_config(dir.path()).unwrap();
        let topo = two_board_topo();
        let board_a = &topo.boards[0];
        let board_b = &topo.boards[1];

        let na = create_node(&config, "dp-a").unwrap();
        let nb = create_node(&config, "dp-b").unwrap();
        let map_a = UartDataPlaneMap::open(&na, &key, "a", board_a, &topo.edges).unwrap();
        let map_b = UartDataPlaneMap::open(&nb, &key, "b", board_b, &topo.edges).unwrap();

        let frame = UartFrame {
            data: 0x41,
            data_bits: 8,
            ..UartFrame::default()
        };

        {
            let (ports_a, _tap_a) = board_ports_with_rx_tap("a_rx_tap");
            let mut plane_a = map_a.bind(&ports_a).unwrap();
            ports_a
                .out_port::<UartFrame>("uart0_tx")
                .unwrap()
                .on_input(&[frame], 0);
            plane_a.pump_tx().unwrap();
        }

        let (ports_b, tap_b) = board_ports_with_rx_tap("b_rx_tap");
        let mut plane_b = map_b.bind(&ports_b).unwrap();

        let mut got = None;
        for _ in 0..100 {
            plane_b.pump_rx().unwrap();
            let pending = tap_b.pending();
            if !pending.is_empty() {
                got = pending.into_iter().next();
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, Some(frame));
    }
}
