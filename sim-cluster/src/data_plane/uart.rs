//! UART frame board-edge data plane.

use std::collections::{HashMap, HashSet};

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::Node;
use iceoryx2::prelude::ipc;
use sim_kernel::{BoardPorts, OutPort, SourcePort, UartFrame};

use crate::ipc::{IpcError, NodeUartPorts, edge_id};
use crate::topology::{BoardSpec, DirectedEdge, EndpointDirection, PayloadKind};

use super::{DataPlane, DataPlaneError, DataPlaneMap};

type UartPub = Publisher<ipc::Service, UartFrame, ()>;
type UartSub = Subscriber<ipc::Service, UartFrame, ()>;

struct UartRxLane {
    sub: UartSub,
    port: SourcePort<UartFrame>,
}

struct UartTxLane {
    port: OutPort<UartFrame>,
    pubs: Vec<UartPub>,
}

struct UartPendingRx {
    edge_id: crate::ipc::HashedId<u64>,
    ep: String,
    sub: UartSub,
}

struct UartPendingTx {
    ep: String,
    edge_ids: Vec<crate::ipc::HashedId<u64>>,
}

/// UART edge↔endpoint mapping opened before board ports exist.
pub struct UartDataPlaneMap {
    producers: HashMap<crate::ipc::HashedId<u64>, UartPub>,
    pending_rx: Vec<UartPendingRx>,
    pending_tx: Vec<UartPendingTx>,
    required_ins: HashSet<String>,
    required_outs: HashSet<String>,
}

impl UartDataPlaneMap {
    pub fn open(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_name: &str,
        board: &BoardSpec,
        topo_edges: &[DirectedEdge],
    ) -> Result<Self, IpcError> {
        let NodeUartPorts {
            mut producers,
            mut consumers,
        } = NodeUartPorts::open_for_board(node, cluster_key, board_name, topo_edges)?;

        let mut pending_rx = Vec::new();
        let mut tx_ep_to_edges: HashMap<String, Vec<crate::ipc::HashedId<u64>>> = HashMap::new();

        for edge in topo_edges {
            if edge.payload != PayloadKind::Uart {
                continue;
            }
            let id = edge_id(
                &edge.from.path,
                &edge.from.port,
                &edge.to.path,
                &edge.to.port,
            );
            match (
                edge.to.path.as_str() == board_name,
                edge.from.path.as_str() == board_name,
            ) {
                (true, _) => {
                    let Some(sub) = consumers.remove(&id) else {
                        return Err(IpcError::Message(format!("missing uart consumer for {id}")));
                    };
                    pending_rx.push(UartPendingRx {
                        edge_id: id,
                        ep: edge.to.port.clone(),
                        sub,
                    });
                }
                (false, true) => {
                    tx_ep_to_edges
                        .entry(edge.from.port.clone())
                        .or_default()
                        .push(id);
                }
                (false, false) => {}
            }
        }

        let pending_tx: Vec<UartPendingTx> = tx_ep_to_edges
            .into_iter()
            .map(|(ep, edge_ids)| UartPendingTx { ep, edge_ids })
            .collect();

        // Keep only producers still needed for pending TX edges.
        let needed: HashSet<crate::ipc::HashedId<u64>> = pending_tx
            .iter()
            .flat_map(|t| t.edge_ids.iter().copied())
            .collect();
        producers.retain(|id, _| needed.contains(id));

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
                        // Drop on failure and continue later frames. iceoryx
                        // SendError here is typically publisher data-segment
                        // exhaustion (fixed SHM pool), not host OOM; with the
                        // default uart_ring_len (~1 ms at 4 µs/frame) that is
                        // rare. Re-queuing would stall the TX path or reorder
                        // characters if later frames were sent first. Board
                        // shutdown on send failure is intentionally avoided.
                        log::warn!("uart tx try_send: {err:?}");
                    }
                }
            }
        }
        Ok(())
    }

    fn discard_rx(&mut self) -> Result<(), DataPlaneError> {
        for lane in &self.rx {
            while lane
                .sub
                .receive()
                .map_err(|e| DataPlaneError::Message(format!("uart recv: {e:?}")))?
                .is_some()
            {}
        }
        Ok(())
    }
}
