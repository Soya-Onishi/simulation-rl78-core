//! Pulse train parameter board-edge data plane.

use std::collections::{HashMap, HashSet};

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::Node;
use iceoryx2::prelude::ipc;
use sim_kernel::{BoardPorts, OutPort, Pulse, SourcePort};

use crate::ipc::{IpcError, NodePulsePorts, edge_id};
use crate::topology::{BoardSpec, DirectedEdge, EndpointDirection, PayloadKind};

use super::{DataPlane, DataPlaneError, DataPlaneMap};

type PulsePub = Publisher<ipc::Service, Pulse, ()>;
type PulseSub = Subscriber<ipc::Service, Pulse, ()>;

struct PulseRxLane {
    sub: PulseSub,
    port: SourcePort<Pulse>,
}

struct PulseTxLane {
    port: OutPort<Pulse>,
    pubs: Vec<PulsePub>,
}

struct PulsePendingRx {
    edge_id: crate::ipc::HashedId<u64>,
    ep: String,
    sub: PulseSub,
}

struct PulsePendingTx {
    ep: String,
    edge_ids: Vec<crate::ipc::HashedId<u64>>,
}

/// Pulse edge↔endpoint mapping opened before board ports exist.
pub struct PulseDataPlaneMap {
    producers: HashMap<crate::ipc::HashedId<u64>, PulsePub>,
    pending_rx: Vec<PulsePendingRx>,
    pending_tx: Vec<PulsePendingTx>,
    required_ins: HashSet<String>,
    required_outs: HashSet<String>,
}

impl PulseDataPlaneMap {
    pub fn open(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_name: &str,
        board: &BoardSpec,
        topo_edges: &[DirectedEdge],
    ) -> Result<Self, IpcError> {
        let NodePulsePorts {
            mut producers,
            mut consumers,
        } = NodePulsePorts::open_for_board(node, cluster_key, board_name, topo_edges)?;

        let mut pending_rx = Vec::new();
        let mut tx_ep_to_edges: HashMap<String, Vec<crate::ipc::HashedId<u64>>> = HashMap::new();

        for edge in topo_edges {
            if edge.payload != PayloadKind::Pulse {
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
                        return Err(IpcError::Message(format!(
                            "missing pulse consumer for {id}"
                        )));
                    };
                    pending_rx.push(PulsePendingRx {
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

        let pending_tx: Vec<PulsePendingTx> = tx_ep_to_edges
            .into_iter()
            .map(|(ep, edge_ids)| PulsePendingTx { ep, edge_ids })
            .collect();

        let needed: HashSet<crate::ipc::HashedId<u64>> = pending_tx
            .iter()
            .flat_map(|t| t.edge_ids.iter().copied())
            .collect();
        producers.retain(|id, _| needed.contains(id));

        let mut required_ins = HashSet::new();
        let mut required_outs = HashSet::new();
        for ep in &board.endpoints {
            if ep.payload != PayloadKind::Pulse {
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

    pub fn bind(mut self, ports: &BoardPorts) -> Result<PulseDataPlane, DataPlaneError> {
        for name in &self.required_ins {
            if ports.in_port::<Pulse>(name).is_none() {
                return Err(DataPlaneError::Message(format!(
                    "missing InPort<Pulse> for topology endpoint `{name}`"
                )));
            }
        }
        for name in &self.required_outs {
            if ports.out_port::<Pulse>(name).is_none() {
                return Err(DataPlaneError::Message(format!(
                    "missing OutPort<Pulse> for topology endpoint `{name}`"
                )));
            }
        }

        let mut rx = Vec::with_capacity(self.pending_rx.len());
        for pending in self.pending_rx {
            let port = ports
                .in_port::<Pulse>(&pending.ep)
                .map(|p| p.port().clone())
                .ok_or_else(|| {
                    DataPlaneError::Message(format!(
                        "pulse rx: no InPort for endpoint `{}` (edge {})",
                        pending.ep, pending.edge_id
                    ))
                })?;
            rx.push(PulseRxLane {
                sub: pending.sub,
                port,
            });
        }

        let mut tx = Vec::with_capacity(self.pending_tx.len());
        for pending in self.pending_tx {
            let port = ports
                .out_port::<Pulse>(&pending.ep)
                .cloned()
                .ok_or_else(|| {
                    DataPlaneError::Message(format!(
                        "pulse tx: no OutPort for endpoint `{}`",
                        pending.ep
                    ))
                })?;
            let mut pubs = Vec::with_capacity(pending.edge_ids.len());
            for edge_id in pending.edge_ids {
                let puber = self.producers.remove(&edge_id).ok_or_else(|| {
                    DataPlaneError::Message(format!("missing pulse producer for {edge_id}"))
                })?;
                pubs.push(puber);
            }
            tx.push(PulseTxLane { port, pubs });
        }

        Ok(PulseDataPlane { rx, tx })
    }
}

impl DataPlaneMap for PulseDataPlaneMap {
    fn bind(self: Box<Self>, ports: &BoardPorts) -> Result<Box<dyn DataPlane>, DataPlaneError> {
        Ok(Box::new((*self).bind(ports)?))
    }
}

/// Bound pulse data plane for one board.
pub struct PulseDataPlane {
    rx: Vec<PulseRxLane>,
    tx: Vec<PulseTxLane>,
}

impl DataPlane for PulseDataPlane {
    fn pump_rx(&mut self) -> Result<(), DataPlaneError> {
        for lane in &self.rx {
            for _ in 0..64 {
                match lane
                    .sub
                    .receive()
                    .map_err(|e| DataPlaneError::Message(format!("pulse recv: {e:?}")))?
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
            for sample in lane.port.drain_pending() {
                for puber in &lane.pubs {
                    if let Err(err) = puber.send_copy(sample) {
                        log::warn!("pulse tx try_send: {err:?}");
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
                .map_err(|e| DataPlaneError::Message(format!("pulse recv: {e:?}")))?
                .is_some()
            {}
        }
        Ok(())
    }
}
