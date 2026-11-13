//! Board data-plane pumping: topology edges ↔ [`BoardPorts`].
//!
//! Control IPC stays in [`crate::board_runtime`]; this module owns payload
//! fan-in/fan-out. Payload kinds live in submodules (`uart`, `analog`, `pulse`,
//! `scalar`, …); [`open_data_planes`] dispatches by [`PayloadKind`].
//!
//! [`open`](UartDataPlaneMap::open) builds edge↔endpoint mapping (and opens
//! iceoryx ports). [`bind`](UartDataPlaneMap::bind) consumes that map with
//! [`BoardPorts`] and yields a [`UartDataPlane`] whose [`pump_rx`] /
//! [`pump_tx`] use pre-wired lanes with no name lookup.

mod analog;
mod pulse;
mod scalar;
mod uart;

pub use analog::{AnalogDataPlane, AnalogDataPlaneMap};
pub use pulse::{PulseDataPlane, PulseDataPlaneMap};
pub use scalar::{ScalarDataPlane, ScalarDataPlaneMap};
pub use uart::{UartDataPlane, UartDataPlaneMap};

use std::collections::HashSet;

use iceoryx2::prelude::Node;
use iceoryx2::prelude::ipc;
use sim_kernel::BoardPorts;

use crate::ipc::IpcError;
use crate::topology::{BoardSpec, LogicalTopology, PayloadKind};

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
    /// Drop queued subscriber samples without driving the guest in-port.
    fn discard_rx(&mut self) -> Result<(), DataPlaneError>;
}

pub fn open_data_planes(
    node: &Node<ipc::Service>,
    cluster_key: &str,
    board_name: &str,
    board: &BoardSpec,
    topo: &LogicalTopology,
) -> Result<Vec<Box<dyn DataPlaneMap>>, DataPlaneError> {
    let mut kinds = HashSet::new();
    for edge in &topo.edges {
        if edge.from.path == board_name || edge.to.path == board_name {
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
                let map =
                    UartDataPlaneMap::open(node, cluster_key, board_name, board, &topo.edges)?;
                maps.push(Box::new(map));
            }
            PayloadKind::Analog => {
                let map =
                    AnalogDataPlaneMap::open(node, cluster_key, board_name, board, &topo.edges)?;
                maps.push(Box::new(map));
            }
            PayloadKind::Pulse => {
                let map =
                    PulseDataPlaneMap::open(node, cluster_key, board_name, board, &topo.edges)?;
                maps.push(Box::new(map));
            }
            PayloadKind::Scalar => {
                let map =
                    ScalarDataPlaneMap::open(node, cluster_key, board_name, board, &topo.edges)?;
                maps.push(Box::new(map));
            }
            PayloadKind::Digital => {
                // Level-only digital IPC remains out of scope; use Pulse for trains.
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

    use sim_kernel::{AnalogVoltage, BoardPorts, InPort, OutPort, Pulse, Scalar, UartFrame, Wire};

    use super::*;
    use crate::ipc::{create_node, isolated_config, new_cluster_key};
    use crate::topology::{
        BoardSpec, DirectedEdge, EndpointDirection, EndpointSpec, LogicalTopology, PayloadKind,
        PortRef,
    };

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
                    from: PortRef {
                        path: "a".into(),
                        port: "uart0_tx".into(),
                    },
                    to: PortRef {
                        path: "b".into(),
                        port: "uart0_rx".into(),
                    },
                    route: vec![
                        PortRef {
                            path: "a".into(),
                            port: "uart0_tx".into(),
                        },
                        PortRef {
                            path: "b".into(),
                            port: "uart0_rx".into(),
                        },
                    ],
                    payload: PayloadKind::Uart,
                    uart_ring_len: Some(8),
                },
                DirectedEdge {
                    from: PortRef {
                        path: "b".into(),
                        port: "uart0_tx".into(),
                    },
                    to: PortRef {
                        path: "a".into(),
                        port: "uart0_rx".into(),
                    },
                    route: vec![
                        PortRef {
                            path: "b".into(),
                            port: "uart0_tx".into(),
                        },
                        PortRef {
                            path: "a".into(),
                            port: "uart0_rx".into(),
                        },
                    ],
                    payload: PayloadKind::Uart,
                    uart_ring_len: Some(8),
                },
            ],
            margin_ns: 1_000,
            booting_timeout_ms: 1_000,
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
    fn uart_dataplane_discards_queued_rx_without_driving_the_guest() {
        let dir = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let config = isolated_config(dir.path()).unwrap();
        let topo = two_board_topo();
        let board_a = &topo.boards[0];
        let board_b = &topo.boards[1];

        let na = create_node(&config, "dp-discard-a").unwrap();
        let nb = create_node(&config, "dp-discard-b").unwrap();
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
        for _ in 0..50 {
            plane_b.discard_rx().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        plane_b.discard_rx().unwrap();
        plane_b.pump_rx().unwrap();
        assert!(
            tap_b.pending().is_empty(),
            "queued uart rx must be dropped before the guest sees it"
        );
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

    fn analog_board(id: &str, out_name: &str, in_name: &str) -> BoardSpec {
        BoardSpec {
            id: id.into(),
            kind: "rl78".into(),
            elf: None,
            endpoints: vec![
                EndpointSpec {
                    name: out_name.into(),
                    direction: EndpointDirection::Out,
                    payload: PayloadKind::Analog,
                },
                EndpointSpec {
                    name: in_name.into(),
                    direction: EndpointDirection::In,
                    payload: PayloadKind::Analog,
                },
            ],
        }
    }

    #[test]
    fn analog_dataplane_tx_ipc_rx_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let config = isolated_config(dir.path()).unwrap();
        let topo = LogicalTopology {
            boards: vec![
                analog_board("a", "dac0", "ain"),
                analog_board("p", "vout", "vin"),
            ],
            edges: vec![DirectedEdge {
                from: PortRef {
                    path: "a".into(),
                    port: "dac0".into(),
                },
                to: PortRef {
                    path: "p".into(),
                    port: "vin".into(),
                },
                route: vec![
                    PortRef {
                        path: "a".into(),
                        port: "dac0".into(),
                    },
                    PortRef {
                        path: "p".into(),
                        port: "vin".into(),
                    },
                ],
                payload: PayloadKind::Analog,
                uart_ring_len: None,
            }],
            margin_ns: 1_000,
            booting_timeout_ms: 1_000,
        };
        let na = create_node(&config, "adp-a").unwrap();
        let np = create_node(&config, "adp-p").unwrap();
        let map_a = AnalogDataPlaneMap::open(&na, &key, "a", &topo.boards[0], &topo.edges).unwrap();
        let map_p = AnalogDataPlaneMap::open(&np, &key, "p", &topo.boards[1], &topo.edges).unwrap();
        let sample = AnalogVoltage(5_000_000);

        {
            let mut ports_a = BoardPorts::new();
            ports_a
                .insert_out(OutPort::<AnalogVoltage>::new("dac0").unwrap())
                .unwrap();
            ports_a
                .insert_in(InPort::<AnalogVoltage>::new("ain").unwrap())
                .unwrap();
            let mut plane_a = map_a.bind(&ports_a).unwrap();
            ports_a
                .out_port::<AnalogVoltage>("dac0")
                .unwrap()
                .on_input(&[sample], 0);
            plane_a.pump_tx().unwrap();
        }

        let mut ports_p = BoardPorts::new();
        let mut vin = InPort::<AnalogVoltage>::new("vin").unwrap();
        let tap = OutPort::<AnalogVoltage>::new("vin_tap").unwrap();
        let _wire = Wire::new().source(vin.port_mut()).sink({
            let tap = tap.clone();
            move |values, changed| tap.on_input(values, changed)
        });
        ports_p.insert_in(vin).unwrap();
        ports_p
            .insert_out(OutPort::<AnalogVoltage>::new("vout").unwrap())
            .unwrap();
        let mut plane_p = map_p.bind(&ports_p).unwrap();
        let mut got = None;
        for _ in 0..100 {
            plane_p.pump_rx().unwrap();
            let pending = tap.pending();
            if !pending.is_empty() {
                got = pending.into_iter().next();
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, Some(sample));
    }

    fn pulse_board(id: &str, out_name: &str, in_name: &str) -> BoardSpec {
        BoardSpec {
            id: id.into(),
            kind: "plant".into(),
            elf: None,
            endpoints: vec![
                EndpointSpec {
                    name: out_name.into(),
                    direction: EndpointDirection::Out,
                    payload: PayloadKind::Pulse,
                },
                EndpointSpec {
                    name: in_name.into(),
                    direction: EndpointDirection::In,
                    payload: PayloadKind::Pulse,
                },
            ],
        }
    }

    #[test]
    fn pulse_dataplane_tx_ipc_rx_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let config = isolated_config(dir.path()).unwrap();
        let topo = LogicalTopology {
            boards: vec![
                pulse_board("plant", "flow_pulse", "unused_in"),
                pulse_board("board", "unused_out", "flow_pulse"),
            ],
            edges: vec![DirectedEdge {
                from: PortRef {
                    path: "plant".into(),
                    port: "flow_pulse".into(),
                },
                to: PortRef {
                    path: "board".into(),
                    port: "flow_pulse".into(),
                },
                route: vec![
                    PortRef {
                        path: "plant".into(),
                        port: "flow_pulse".into(),
                    },
                    PortRef {
                        path: "board".into(),
                        port: "flow_pulse".into(),
                    },
                ],
                payload: PayloadKind::Pulse,
                uart_ring_len: None,
            }],
            margin_ns: 1_000,
            booting_timeout_ms: 1_000,
        };
        let n_plant = create_node(&config, "pdp-plant").unwrap();
        let n_board = create_node(&config, "pdp-board").unwrap();
        let map_plant =
            PulseDataPlaneMap::open(&n_plant, &key, "plant", &topo.boards[0], &topo.edges).unwrap();
        let map_board =
            PulseDataPlaneMap::open(&n_board, &key, "board", &topo.boards[1], &topo.edges).unwrap();
        let sample = Pulse {
            hi_width_ns: 1_000_000,
            lo_width_ns: 1_000_000,
        };

        {
            let mut ports_plant = BoardPorts::new();
            ports_plant
                .insert_out(OutPort::<Pulse>::new("flow_pulse").unwrap())
                .unwrap();
            ports_plant
                .insert_in(InPort::<Pulse>::new("unused_in").unwrap())
                .unwrap();
            let mut plane_plant = map_plant.bind(&ports_plant).unwrap();
            ports_plant
                .out_port::<Pulse>("flow_pulse")
                .unwrap()
                .on_input(&[sample], 0);
            plane_plant.pump_tx().unwrap();
        }

        let mut ports_board = BoardPorts::new();
        let mut flow_in = InPort::<Pulse>::new("flow_pulse").unwrap();
        let tap = OutPort::<Pulse>::new("flow_tap").unwrap();
        let _wire = Wire::new().source(flow_in.port_mut()).sink({
            let tap = tap.clone();
            move |values, changed| tap.on_input(values, changed)
        });
        ports_board.insert_in(flow_in).unwrap();
        ports_board
            .insert_out(OutPort::<Pulse>::new("unused_out").unwrap())
            .unwrap();
        let mut plane_board = map_board.bind(&ports_board).unwrap();
        let mut got = None;
        for _ in 0..100 {
            plane_board.pump_rx().unwrap();
            let pending = tap.pending();
            if !pending.is_empty() {
                got = pending.into_iter().next();
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, Some(sample));
    }

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
    fn scalar_dataplane_tx_ipc_rx_round_trip() {
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
        let map_piping =
            ScalarDataPlaneMap::open(&n_piping, &key, "piping", &topo.boards[0], &topo.edges)
                .unwrap();
        let map_boiler =
            ScalarDataPlaneMap::open(&n_boiler, &key, "boiler", &topo.boards[1], &topo.edges)
                .unwrap();
        let sample = Scalar(20.0);

        {
            let mut ports_piping = BoardPorts::new();
            ports_piping
                .insert_out(OutPort::<Scalar>::new("inlet_temp_c").unwrap())
                .unwrap();
            ports_piping
                .insert_in(InPort::<Scalar>::new("outlet_temp_c").unwrap())
                .unwrap();
            let mut plane_piping = map_piping.bind(&ports_piping).unwrap();
            ports_piping
                .out_port::<Scalar>("inlet_temp_c")
                .unwrap()
                .on_input(&[sample], 0);
            plane_piping.pump_tx().unwrap();
        }

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
        let mut plane_boiler = map_boiler.bind(&ports_boiler).unwrap();
        let mut got = None;
        for _ in 0..100 {
            plane_boiler.pump_rx().unwrap();
            let pending = tap.pending();
            if !pending.is_empty() {
                got = pending.into_iter().next();
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, Some(sample));
    }
}
