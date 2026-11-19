//! Open board-edge lanes from topology + [`BoardPorts`].
//!
//! Pub/sub creation lives in [`sim_kernel::dataplane`]. This module only walks
//! topology edges, builds [`OpenCtx`], and calls [`ExternPort::open`].

use std::any::TypeId;
use std::collections::{HashMap, HashSet};

use iceoryx2::prelude::Node;
use iceoryx2::prelude::ipc;
use sim_kernel::{
    AnalogVoltage, BoardPorts, DataPlane, DigitalLevel, EdgeSpec, ExternPort, Lane, OpenCtx,
    OpenError, Pulse, PumpError, RxLane, Scalar, TxLane, UartFrame,
};

use crate::ipc::{IpcError, edge as edge_service, edge_id};
use crate::topology::{
    BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, LogicalTopology, PayloadKind,
};

/// Failures from lane open / pump.
#[derive(Debug, thiserror::Error)]
pub enum DataPlaneError {
    #[error("{0}")]
    Message(String),
}

impl From<OpenError> for DataPlaneError {
    fn from(value: OpenError) -> Self {
        Self::Message(value.to_string())
    }
}

impl From<PumpError> for DataPlaneError {
    fn from(value: PumpError) -> Self {
        Self::Message(value.to_string())
    }
}

impl From<IpcError> for DataPlaneError {
    fn from(value: IpcError) -> Self {
        Self::Message(value.to_string())
    }
}

fn ring_capacity(edge: &DirectedEdge) -> usize {
    match edge.payload {
        PayloadKind::Uart => LogicalTopology::uart_ring_len(edge).max(2) as usize,
        PayloadKind::Pulse => Pulse::DEFAULT_RING_CAPACITY,
        PayloadKind::Analog => AnalogVoltage::DEFAULT_RING_CAPACITY,
        PayloadKind::Scalar => Scalar::DEFAULT_RING_CAPACITY,
        PayloadKind::Digital => DigitalLevel::DEFAULT_RING_CAPACITY,
    }
}

fn expected_payload_type_id(kind: PayloadKind) -> TypeId {
    match kind {
        PayloadKind::Uart => TypeId::of::<UartFrame>(),
        PayloadKind::Analog => TypeId::of::<AnalogVoltage>(),
        PayloadKind::Pulse => TypeId::of::<Pulse>(),
        PayloadKind::Scalar => TypeId::of::<Scalar>(),
        PayloadKind::Digital => TypeId::of::<DigitalLevel>(),
    }
}

fn payload_kind_name(kind: PayloadKind) -> &'static str {
    match kind {
        PayloadKind::Uart => "uart",
        PayloadKind::Analog => "analog",
        PayloadKind::Pulse => "pulse",
        PayloadKind::Scalar => "scalar",
        PayloadKind::Digital => "digital",
    }
}

/// Per-edge iceoryx services are SPSC; topology always yields 1/1 today.
fn edge_qos() -> (usize, usize) {
    (1, 1)
}

fn push_ordered(
    order: &mut Vec<String>,
    map: &mut HashMap<String, Vec<EdgeSpec>>,
    ep: String,
    spec: EdgeSpec,
) {
    if !map.contains_key(&ep) {
        order.push(ep.clone());
    }
    map.entry(ep).or_default().push(spec);
}

/// Open iceoryx lanes for every board endpoint that has topology edges.
///
/// Consumes matching entries from `ports` via [`BoardPorts::remove`]. Leftover
/// ports (no edges) remain in `ports` until the caller drops it.
///
/// Lane order follows first appearance of each endpoint in `topo.edges`.
///
/// Validates each opened endpoint against [`BoardSpec`]: name presence,
/// [`EndpointDirection`] vs In/Out, and Rust payload `TypeId` vs
/// [`EndpointSpec::payload`].
pub fn open_lanes(
    node: &Node<ipc::Service>,
    cluster_key: &str,
    board_name: &str,
    board: &BoardSpec,
    topo: &LogicalTopology,
    ports: &mut BoardPorts,
) -> Result<(Vec<RxLane>, Vec<TxLane>), DataPlaneError> {
    let ep_by_name: HashMap<&str, &EndpointSpec> = board
        .endpoints
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();

    let mut rx_edges: HashMap<String, Vec<EdgeSpec>> = HashMap::new();
    let mut tx_edges: HashMap<String, Vec<EdgeSpec>> = HashMap::new();
    let mut rx_order = Vec::new();
    let mut tx_order = Vec::new();
    let (max_publishers, max_subscribers) = edge_qos();

    for edge in &topo.edges {
        let id = edge_id(
            &edge.from.path,
            &edge.from.port,
            &edge.to.path,
            &edge.to.port,
        );
        let spec = EdgeSpec {
            service_name: edge_service(cluster_key, id),
            max_publishers,
            max_subscribers,
            ring_capacity: ring_capacity(edge),
        };
        if edge.to.path == board_name {
            let ep = ep_by_name.get(edge.to.port.as_str()).ok_or_else(|| {
                DataPlaneError::Message(format!(
                    "board `{board_name}` has no endpoint `{}` for inbound edge",
                    edge.to.port
                ))
            })?;
            if ep.payload != edge.payload {
                return Err(DataPlaneError::Message(format!(
                    "board `{board_name}` endpoint `{}` payload {:?} != edge {:?}",
                    ep.name, ep.payload, edge.payload
                )));
            }
            push_ordered(
                &mut rx_order,
                &mut rx_edges,
                edge.to.port.clone(),
                spec.clone(),
            );
        }
        if edge.from.path == board_name {
            let ep = ep_by_name.get(edge.from.port.as_str()).ok_or_else(|| {
                DataPlaneError::Message(format!(
                    "board `{board_name}` has no endpoint `{}` for outbound edge",
                    edge.from.port
                ))
            })?;
            if ep.payload != edge.payload {
                return Err(DataPlaneError::Message(format!(
                    "board `{board_name}` endpoint `{}` payload {:?} != edge {:?}",
                    ep.name, ep.payload, edge.payload
                )));
            }
            push_ordered(&mut tx_order, &mut tx_edges, edge.from.port.clone(), spec);
        }
    }

    let port_names: HashSet<&str> = ports.port_names().into_iter().collect();
    for ep in &board.endpoints {
        let needed = rx_edges.contains_key(&ep.name) || tx_edges.contains_key(&ep.name);
        if needed && !port_names.contains(ep.name.as_str()) {
            return Err(DataPlaneError::Message(format!(
                "board `{board_name}` endpoint `{}` has edges but no BoardPorts entry",
                ep.name
            )));
        }
    }

    let mut rx = Vec::new();
    let mut tx = Vec::new();

    for ep_name in rx_order {
        let edges = rx_edges.remove(&ep_name).expect("rx order tracks map");
        let ep = ep_by_name[&ep_name.as_str()];
        let port = ports.remove(&ep_name).ok_or_else(|| {
            DataPlaneError::Message(format!(
                "missing InPort for topology endpoint `{ep_name}` on board `{board_name}`"
            ))
        })?;
        open_rx(node, board_name, ep, port, edges, &mut rx)?;
    }

    for ep_name in tx_order {
        let edges = tx_edges.remove(&ep_name).expect("tx order tracks map");
        let ep = ep_by_name[&ep_name.as_str()];
        let port = ports.remove(&ep_name).ok_or_else(|| {
            DataPlaneError::Message(format!(
                "missing OutPort for topology endpoint `{ep_name}` on board `{board_name}`"
            ))
        })?;
        open_tx(node, board_name, ep, port, edges, &mut tx)?;
    }

    Ok((rx, tx))
}

fn check_port(
    board_name: &str,
    ep: &EndpointSpec,
    port: &dyn ExternPort,
    expect_input: bool,
) -> Result<(), DataPlaneError> {
    if port.is_input() != expect_input {
        let want = if expect_input { "InPort" } else { "OutPort" };
        let got = if port.is_input() { "InPort" } else { "OutPort" };
        return Err(DataPlaneError::Message(format!(
            "endpoint `{}` on `{board_name}` expects {want} but BoardPorts has {got}",
            ep.name
        )));
    }
    if ep.direction == EndpointDirection::In && !expect_input {
        return Err(DataPlaneError::Message(format!(
            "endpoint `{}` on `{board_name}` is direction in but opened as TX",
            ep.name
        )));
    }
    if ep.direction == EndpointDirection::Out && expect_input {
        return Err(DataPlaneError::Message(format!(
            "endpoint `{}` on `{board_name}` is direction out but opened as RX",
            ep.name
        )));
    }
    let expected = expected_payload_type_id(ep.payload);
    if port.payload_type_id() != expected {
        return Err(DataPlaneError::Message(format!(
            "endpoint `{}` on `{board_name}` expects payload {} but BoardPorts type differs",
            ep.name,
            payload_kind_name(ep.payload)
        )));
    }
    Ok(())
}

fn open_rx(
    node: &Node<ipc::Service>,
    board_name: &str,
    ep: &EndpointSpec,
    port: Box<dyn ExternPort>,
    edges: Vec<EdgeSpec>,
    out: &mut Vec<RxLane>,
) -> Result<(), DataPlaneError> {
    check_port(board_name, ep, port.as_ref(), true)?;
    match port.open(OpenCtx { node, edges })? {
        Lane::Rx(lane) => {
            out.push(lane);
            Ok(())
        }
        Lane::Tx(_) => Err(DataPlaneError::Message(format!(
            "endpoint `{}` on `{board_name}` is inbound but opened as Tx",
            ep.name
        ))),
    }
}

fn open_tx(
    node: &Node<ipc::Service>,
    board_name: &str,
    ep: &EndpointSpec,
    port: Box<dyn ExternPort>,
    edges: Vec<EdgeSpec>,
    out: &mut Vec<TxLane>,
) -> Result<(), DataPlaneError> {
    check_port(board_name, ep, port.as_ref(), false)?;
    match port.open(OpenCtx { node, edges })? {
        Lane::Tx(lane) => {
            out.push(lane);
            Ok(())
        }
        Lane::Rx(_) => Err(DataPlaneError::Message(format!(
            "endpoint `{}` on `{board_name}` is outbound but opened as Rx",
            ep.name
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use sim_kernel::{InPort, OutPort, Wire};

    use crate::ipc::{create_node, isolated_config, new_cluster_key};
    use crate::topology::PortRef;

    fn scalar_board(id: &str, out_name: &str, in_name: &str) -> BoardSpec {
        BoardSpec {
            id: id.into(),
            kind: "plant".into(),
            elf: None,
            endpoints: vec![
                EndpointSpec {
                    name: out_name.into(),
                    direction: EndpointDirection::Out,
                    payload: PayloadKind::Scalar,
                },
                EndpointSpec {
                    name: in_name.into(),
                    direction: EndpointDirection::In,
                    payload: PayloadKind::Scalar,
                },
            ],
        }
    }

    #[test]
    fn scalar_open_lanes_tx_ipc_rx_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let config = isolated_config(dir.path()).unwrap();
        let topo = LogicalTopology {
            boards: vec![
                scalar_board("piping", "inlet_temp_c", "outlet_temp_c"),
                scalar_board("boiler", "outlet_temp_c", "inlet_temp_c"),
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
            margin_ns: 1_000,
            booting_timeout_ms: 1_000,
        };
        let n_piping = create_node(&config, "sdp-piping").unwrap();
        let n_boiler = create_node(&config, "sdp-boiler").unwrap();
        let sample = Scalar(20.0);

        // Open RX first so the subscriber is connected before TX publishes.
        let mut ports_boiler = BoardPorts::new();
        let mut inlet = InPort::<Scalar>::new("inlet_temp_c").unwrap();
        let tap = OutPort::<Scalar>::new("inlet_tap").unwrap();
        let _wire = Wire::new().source(inlet.port_mut()).sink({
            let tap = tap.clone();
            move |values, changed| tap.on_input(values, changed)
        });
        ports_boiler.insert_in(inlet).unwrap();
        ports_boiler
            .insert_out(OutPort::<Scalar>::new("outlet_temp_c").unwrap())
            .unwrap();
        let (mut rx_lanes, _tx_boiler) = open_lanes(
            &n_boiler,
            &key,
            "boiler",
            &topo.boards[1],
            &topo,
            &mut ports_boiler,
        )
        .unwrap();
        assert_eq!(rx_lanes.len(), 1);

        let mut ports_piping = BoardPorts::new();
        let tx = OutPort::<Scalar>::new("inlet_temp_c").unwrap();
        let drive = tx.clone();
        ports_piping.insert_out(tx).unwrap();
        ports_piping
            .insert_in(InPort::<Scalar>::new("outlet_temp_c").unwrap())
            .unwrap();
        let (_rx_piping, mut tx_lanes) = open_lanes(
            &n_piping,
            &key,
            "piping",
            &topo.boards[0],
            &topo,
            &mut ports_piping,
        )
        .unwrap();
        assert_eq!(tx_lanes.len(), 1);
        drive.on_input(&[sample], 0);
        tx_lanes[0].pump().unwrap();

        let mut got = None;
        for _ in 0..100 {
            rx_lanes[0].pump().unwrap();
            let pending = tap.pending();
            if !pending.is_empty() {
                got = pending.into_iter().next();
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, Some(sample));
    }

    #[test]
    fn open_lanes_rejects_payload_type_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let config = isolated_config(dir.path()).unwrap();
        let board = scalar_board("piping", "inlet_temp_c", "outlet_temp_c");
        let topo = LogicalTopology {
            boards: vec![board.clone(), scalar_board("boiler", "x", "inlet_temp_c")],
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
            margin_ns: 1_000,
            booting_timeout_ms: 1_000,
        };
        let node = create_node(&config, "mismatch").unwrap();
        let mut ports = BoardPorts::new();
        // Wrong payload type for Scalar endpoint.
        ports
            .insert_out(OutPort::<AnalogVoltage>::new("inlet_temp_c").unwrap())
            .unwrap();
        ports
            .insert_in(InPort::<Scalar>::new("outlet_temp_c").unwrap())
            .unwrap();
        let err = match open_lanes(&node, &key, "piping", &board, &topo, &mut ports) {
            Err(e) => e,
            Ok(_) => panic!("expected payload type mismatch"),
        };
        assert!(err.to_string().contains("expects payload scalar"), "{err}");
    }
}
