//! UDP multicast between arbiters: virtual-time mins, Cluster Stop, and
//! cross-host endpoint samples (p2).
//!
//! Wire format is fixed binary (not JSON). Endpoint sample bodies match the
//! iceoryx `repr(C)` layouts for [`sim_kernel::UartFrame`],
//! [`sim_kernel::AnalogVoltage`], [`sim_kernel::Pulse`], and
//! [`sim_kernel::Scalar`].

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::config::MachineId;
use crate::control::HostStopReason;
use crate::topology::{DirectedEdge, PayloadKind};

/// Magic bytes for sim-cluster multicast datagrams.
const MAGIC: [u8; 4] = *b"SCMC";
const VERSION: u8 = 1;

/// UDP recv buffer; must fit Endpoint headers + largest iceoryx sample.
const RECV_BUF_LEN: usize = 256;

/// Default keepalive / resend interval (wall clock), per design.
pub const DEFAULT_MAX_INTERVAL: Duration = Duration::from_millis(100);

/// Wire `payload_kind` for [`MulticastMessage::Endpoint`].
pub const ENDPOINT_KIND_UART: u8 = 1;
pub const ENDPOINT_KIND_ANALOG: u8 = 2;
pub const ENDPOINT_KIND_PULSE: u8 = 3;
pub const ENDPOINT_KIND_SCALAR: u8 = 4;

/// Multicast message kinds.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MulticastMsgKind {
    VtMin = 1,
    ClusterStop = 2,
    Endpoint = 3,
}

impl MulticastMsgKind {
    #[must_use]
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::VtMin),
            2 => Some(Self::ClusterStop),
            3 => Some(Self::Endpoint),
            _ => None,
        }
    }
}

/// Decoded multicast payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MulticastMessage {
    /// Sender's local `min_board_vt`.
    VtMin {
        machine_id: MachineId,
        min_vt_ns: u64,
    },
    ClusterStop {
        machine_id: MachineId,
        reason: HostStopReason,
    },
    /// Cross-host board-edge sample (p2).
    Endpoint {
        machine_id: MachineId,
        edge_id: u64,
        payload_kind: u8,
        payload: Vec<u8>,
    },
}

impl MulticastMessage {
    /// Encode to a UDP datagram body.
    ///
    /// Layout: `SCMC` magic, version `u8`, kind `u8`, then kind-specific fields.
    /// `machine_id` is big-endian (IPv4 wire order); `min_vt_ns` is little-endian.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 1 + 1 + 4 + 8);
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        match self {
            Self::VtMin {
                machine_id,
                min_vt_ns,
            } => {
                out.push(MulticastMsgKind::VtMin as u8);
                out.extend_from_slice(&machine_id.to_be_bytes());
                out.extend_from_slice(&min_vt_ns.to_le_bytes());
            }
            Self::ClusterStop { machine_id, reason } => {
                out.push(MulticastMsgKind::ClusterStop as u8);
                out.extend_from_slice(&machine_id.to_be_bytes());
                out.push(reason.to_wire());
            }
            Self::Endpoint {
                machine_id,
                edge_id,
                payload_kind,
                payload,
            } => {
                out.push(MulticastMsgKind::Endpoint as u8);
                out.extend_from_slice(&machine_id.to_be_bytes());
                out.extend_from_slice(&edge_id.to_le_bytes());
                out.push(*payload_kind);
                out.extend_from_slice(payload);
            }
        }
        out
    }

    /// `None` if the datagram is not a well-formed multicast message.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(&MAGIC)?;
        let (&version, rest) = rest.split_first()?;
        if version != VERSION {
            return None;
        }
        let (&kind, rest) = rest.split_first()?;
        let kind = MulticastMsgKind::from_u8(kind)?;
        match kind {
            MulticastMsgKind::VtMin => {
                if rest.len() < 12 {
                    return None;
                }
                let machine_id = MachineId::from_be_bytes(rest[0..4].try_into().ok()?);
                let min_vt_ns = u64::from_le_bytes(rest[4..12].try_into().ok()?);
                Some(Self::VtMin {
                    machine_id,
                    min_vt_ns,
                })
            }
            MulticastMsgKind::ClusterStop => {
                if rest.len() < 5 {
                    return None;
                }
                let machine_id = MachineId::from_be_bytes(rest[0..4].try_into().ok()?);
                let reason = HostStopReason::from_wire(rest[4])?;
                Some(Self::ClusterStop { machine_id, reason })
            }
            MulticastMsgKind::Endpoint => {
                // machine_id(4) + edge_id(8) + payload_kind(1) + payload
                if rest.len() < 13 {
                    return None;
                }
                let machine_id = MachineId::from_be_bytes(rest[0..4].try_into().ok()?);
                let edge_id = u64::from_le_bytes(rest[4..12].try_into().ok()?);
                let payload_kind = rest[12];
                let payload = rest[13..].to_vec();
                if expected_endpoint_payload_len(payload_kind)? != payload.len() {
                    return None;
                }
                if !validate_endpoint_payload(payload_kind, &payload) {
                    return None;
                }
                Some(Self::Endpoint {
                    machine_id,
                    edge_id,
                    payload_kind,
                    payload,
                })
            }
        }
    }
}

/// Fixed Endpoint sample body sizes (field-wise LE layout; padding bytes are 0).
/// Matches `repr(C)` sizes of the iceoryx sample types on this target.
pub const ENDPOINT_UART_LEN: usize = 24;
pub const ENDPOINT_ANALOG_LEN: usize = 8;
pub const ENDPOINT_PULSE_LEN: usize = 16;
pub const ENDPOINT_SCALAR_LEN: usize = 8;

const _: () = assert!(std::mem::size_of::<sim_kernel::UartFrame>() == ENDPOINT_UART_LEN);
const _: () = assert!(std::mem::size_of::<sim_kernel::AnalogVoltage>() == ENDPOINT_ANALOG_LEN);
const _: () = assert!(std::mem::size_of::<sim_kernel::Pulse>() == ENDPOINT_PULSE_LEN);
const _: () = assert!(std::mem::size_of::<sim_kernel::Scalar>() == ENDPOINT_SCALAR_LEN);
const _: () = assert!(6 + 13 + ENDPOINT_UART_LEN <= RECV_BUF_LEN);

/// Expected sample body length for an Endpoint `payload_kind`, or `None` if unknown.
#[must_use]
pub fn expected_endpoint_payload_len(payload_kind: u8) -> Option<usize> {
    match payload_kind {
        ENDPOINT_KIND_UART => Some(ENDPOINT_UART_LEN),
        ENDPOINT_KIND_ANALOG => Some(ENDPOINT_ANALOG_LEN),
        ENDPOINT_KIND_PULSE => Some(ENDPOINT_PULSE_LEN),
        ENDPOINT_KIND_SCALAR => Some(ENDPOINT_SCALAR_LEN),
        _ => None,
    }
}

/// Wire `payload_kind` for a topology [`PayloadKind`], if forwardable in p2.
#[must_use]
pub fn endpoint_payload_kind_wire(kind: PayloadKind) -> Option<u8> {
    match kind {
        PayloadKind::Uart => Some(ENDPOINT_KIND_UART),
        PayloadKind::Analog => Some(ENDPOINT_KIND_ANALOG),
        PayloadKind::Pulse => Some(ENDPOINT_KIND_PULSE),
        PayloadKind::Scalar => Some(ENDPOINT_KIND_SCALAR),
        PayloadKind::Digital => None,
    }
}

/// Encode a UART sample for the Endpoint body (LE fields, zero padding).
#[must_use]
pub fn encode_uart_payload(frame: &sim_kernel::UartFrame) -> Vec<u8> {
    let mut out = vec![0u8; ENDPOINT_UART_LEN];
    out[0..2].copy_from_slice(&frame.data.to_le_bytes());
    out[2] = frame.data_bits;
    // out[3..8] padding
    out[8..16].copy_from_slice(&frame.bit_time.0.to_le_bytes());
    out[16] = frame.stop_bits as u8;
    out[17] = frame.parity as u8;
    out[18] = u8::from(frame.inverted);
    out
}

/// Decode + validate a UART Endpoint body. Rejects invalid enum/bool values.
#[must_use]
pub fn decode_uart_payload(bytes: &[u8]) -> Option<sim_kernel::UartFrame> {
    if bytes.len() != ENDPOINT_UART_LEN {
        return None;
    }
    let data = u16::from_le_bytes(bytes[0..2].try_into().ok()?);
    let data_bits = bytes[2];
    let bit_time = sim_kernel::Tick(u64::from_le_bytes(bytes[8..16].try_into().ok()?));
    let stop_bits = match bytes[16] {
        0 => sim_kernel::UartStopBits::One,
        1 => sim_kernel::UartStopBits::Two,
        _ => return None,
    };
    let parity = match bytes[17] {
        0 => sim_kernel::UartParity::None,
        1 => sim_kernel::UartParity::Even,
        2 => sim_kernel::UartParity::Odd,
        _ => return None,
    };
    let inverted = match bytes[18] {
        0 => false,
        1 => true,
        _ => return None,
    };
    Some(sim_kernel::UartFrame {
        data,
        data_bits,
        bit_time,
        stop_bits,
        parity,
        inverted,
    })
}

#[must_use]
pub fn encode_analog_payload(v: &sim_kernel::AnalogVoltage) -> Vec<u8> {
    v.0.to_le_bytes().to_vec()
}

#[must_use]
pub fn decode_analog_payload(bytes: &[u8]) -> Option<sim_kernel::AnalogVoltage> {
    Some(sim_kernel::AnalogVoltage(i64::from_le_bytes(
        bytes.try_into().ok()?,
    )))
}

#[must_use]
pub fn encode_pulse_payload(p: &sim_kernel::Pulse) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENDPOINT_PULSE_LEN);
    out.extend_from_slice(&p.hi_width_ns.to_le_bytes());
    out.extend_from_slice(&p.lo_width_ns.to_le_bytes());
    out
}

#[must_use]
pub fn decode_pulse_payload(bytes: &[u8]) -> Option<sim_kernel::Pulse> {
    if bytes.len() != ENDPOINT_PULSE_LEN {
        return None;
    }
    Some(sim_kernel::Pulse {
        hi_width_ns: u64::from_le_bytes(bytes[0..8].try_into().ok()?),
        lo_width_ns: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
    })
}

#[must_use]
pub fn encode_scalar_payload(s: &sim_kernel::Scalar) -> Vec<u8> {
    s.0.to_le_bytes().to_vec()
}

#[must_use]
pub fn decode_scalar_payload(bytes: &[u8]) -> Option<sim_kernel::Scalar> {
    Some(sim_kernel::Scalar(f64::from_le_bytes(
        bytes.try_into().ok()?,
    )))
}

/// True when `payload` is a well-formed body for `payload_kind`.
#[must_use]
pub fn validate_endpoint_payload(payload_kind: u8, payload: &[u8]) -> bool {
    match payload_kind {
        ENDPOINT_KIND_UART => decode_uart_payload(payload).is_some(),
        ENDPOINT_KIND_ANALOG => decode_analog_payload(payload).is_some(),
        ENDPOINT_KIND_PULSE => decode_pulse_payload(payload).is_some(),
        ENDPOINT_KIND_SCALAR => decode_scalar_payload(payload).is_some(),
        _ => false,
    }
}

/// Joined UDP multicast group for one simulation.
pub struct ClusterMulticast {
    socket: UdpSocket,
    group: SocketAddrV4,
    machine_id: MachineId,
}

impl ClusterMulticast {
    /// Bind and join `group`.
    ///
    /// `interface` is [`crate::config::BackendConfig::addr`] (parsed IPv4): used
    /// both for group membership and as the multicast send interface
    /// (`IP_MULTICAST_IF`), so multi-NIC hosts egress on the cluster address.
    ///
    /// Multicast loopback is disabled so this host does not re-receive its own
    /// datagrams (one `cluster-backend` per machine; avoids extra decode work).
    /// Linux defaults `IP_MULTICAST_LOOP` to on, so this is set explicitly.
    pub fn join(
        group: SocketAddrV4,
        machine_id: MachineId,
        interface: Ipv4Addr,
    ) -> io::Result<Self> {
        // Bind the multicast group address+port so datagrams destined to the
        // group are delivered here. `interface` (BackendConfig::addr) selects
        // the NIC for IP_MULTICAST_IF (send) and IP_ADD_MEMBERSHIP (join).
        let bind_addr = SocketAddrV4::new(*group.ip(), group.port());
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.bind(&SockAddr::from(bind_addr))?;
        socket.set_multicast_if_v4(&interface)?;
        let socket: UdpSocket = socket.into();
        socket.set_nonblocking(true)?;
        socket.set_multicast_loop_v4(false)?;
        socket.set_multicast_ttl_v4(1)?;
        socket.join_multicast_v4(group.ip(), &interface)?;
        Ok(Self {
            socket,
            group,
            machine_id,
        })
    }

    #[must_use]
    pub fn machine_id(&self) -> MachineId {
        self.machine_id
    }

    #[must_use]
    pub fn group(&self) -> SocketAddrV4 {
        self.group
    }

    pub fn send(&self, message: &MulticastMessage) -> io::Result<()> {
        let bytes = message.encode();
        self.socket.send_to(&bytes, SocketAddr::V4(self.group))?;
        Ok(())
    }

    /// Non-blocking receive; `Ok(None)` when no datagram is ready.
    ///
    /// Malformed datagrams are skipped so a junk packet does not look like
    /// an empty socket (which would stop [`Self::recv_all`] early).
    pub fn try_recv(&self) -> io::Result<Option<MulticastMessage>> {
        let mut buf = [0u8; RECV_BUF_LEN];
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((n, _)) => {
                    if let Some(msg) = MulticastMessage::decode(&buf[..n]) {
                        return Ok(Some(msg));
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(err) => return Err(err),
            }
        }
    }

    /// Drain the socket and return all well-formed messages.
    pub fn recv_all(&self) -> io::Result<Vec<MulticastMessage>> {
        let mut out = Vec::new();
        loop {
            match self.try_recv()? {
                Some(msg) => out.push(msg),
                None => break,
            }
        }
        Ok(out)
    }
}

/// Per-remote-host latest min VT; slot-0 input is the min of these values.
#[derive(Clone, Debug, Default)]
pub struct RemoteVtTable {
    /// `machine_id` → latest reported min VT (excludes local machine).
    peers: HashMap<MachineId, u64>,
}

impl RemoteVtTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update one remote; returns the new aggregate min for slot 0 (`u64::MAX` if empty).
    pub fn upsert(&mut self, machine_id: MachineId, min_vt_ns: u64) -> u64 {
        self.peers.insert(machine_id, min_vt_ns);
        self.aggregate_min()
    }

    /// Minimum over remotes, or `u64::MAX` when no peers have reported.
    #[must_use]
    pub fn aggregate_min(&self) -> u64 {
        self.peers.values().copied().min().unwrap_or(u64::MAX)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

/// When to multicast the local min VT (design: VT delta OR wall-clock interval).
#[derive(Clone, Debug)]
pub struct VtPublishThrottle {
    last_sent_vt: Option<u64>,
    last_sent_at: Option<Instant>,
    vt_delta_ns: u64,
    max_interval: Duration,
}

impl VtPublishThrottle {
    #[must_use]
    pub fn new(margin_ns: u64) -> Self {
        Self {
            last_sent_vt: None,
            last_sent_at: None,
            vt_delta_ns: margin_ns / 2,
            max_interval: DEFAULT_MAX_INTERVAL,
        }
    }

    #[must_use]
    pub fn with_interval(mut self, max_interval: Duration) -> Self {
        self.max_interval = max_interval;
        self
    }

    /// Whether a send should happen now for `current_min_vt`.
    #[must_use]
    pub fn should_send(&self, current_min_vt: u64, now: Instant) -> bool {
        let Some(last_vt) = self.last_sent_vt else {
            return true;
        };
        let Some(last_at) = self.last_sent_at else {
            return true;
        };
        let vt_advanced = current_min_vt.saturating_sub(last_vt) >= self.vt_delta_ns;
        let timed_out = now.duration_since(last_at) >= self.max_interval;
        vt_advanced || timed_out
    }

    pub fn record_send(&mut self, current_min_vt: u64, now: Instant) {
        self.last_sent_vt = Some(current_min_vt);
        self.last_sent_at = Some(now);
    }
}

/// Allocate the next free `(address, port)` from configured ranges.
///
/// `used` holds pairs already assigned by this frontend.
#[must_use]
pub fn allocate_multicast(
    address_ranges: &[(Ipv4Addr, Ipv4Addr)],
    port_ranges: &[(u16, u16)],
    used: &[(Ipv4Addr, u16)],
) -> Option<(Ipv4Addr, u16)> {
    for &(a0, a1) in address_ranges {
        let start = u32::from(a0);
        let end = u32::from(a1);
        for raw in start..=end {
            let addr = Ipv4Addr::from(raw);
            for &(p0, p1) in port_ranges {
                for port in p0..=p1 {
                    if !used
                        .iter()
                        .any(|&(u_addr, u_port)| u_addr == addr && u_port == port)
                    {
                        return Some((addr, port));
                    }
                }
            }
        }
    }
    None
}

/// First-Fit board placement onto backends keyed by [`MachineId`].
///
/// Boards are taken in `board_ids` order. For each board, backends are tried in
/// [`HashMap::iter`] order over `remaining` (not YAML `servers[]` order).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardPlacement {
    /// `board_id` → backend [`MachineId`].
    pub board_to_server: Vec<(String, MachineId)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlacementError {
    InsufficientCapacity { boards: usize, free_slots: u64 },
}

impl std::fmt::Display for PlacementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientCapacity { boards, free_slots } => write!(
                f,
                "not enough board capacity: need {boards} boards, free slots {free_slots}"
            ),
        }
    }
}

impl std::error::Error for PlacementError {}

/// Place `board_ids` with First-Fit over `remaining` ([`MachineId`] → free slots).
/// Does not mutate `remaining`; tracks taken slots in a local map.
pub fn place_boards_first_fit(
    board_ids: &[String],
    remaining: &HashMap<MachineId, u32>,
) -> Result<BoardPlacement, PlacementError> {
    let free_slots: u64 = remaining.values().map(|slots| u64::from(*slots)).sum();
    let need = board_ids.len() as u64;
    if need > free_slots {
        return Err(PlacementError::InsufficientCapacity {
            boards: board_ids.len(),
            free_slots,
        });
    }
    let mut taken: HashMap<MachineId, u32> = HashMap::new();
    let mut board_to_server = Vec::with_capacity(board_ids.len());
    for board_id in board_ids {
        let Some(machine_id) = remaining.iter().find_map(|(id, &slots)| {
            let used = taken.get(id).copied().unwrap_or(0);
            (used < slots).then_some(*id)
        }) else {
            return Err(PlacementError::InsufficientCapacity {
                boards: board_ids.len(),
                free_slots: 0,
            });
        };
        *taken.entry(machine_id).or_insert(0) += 1;
        board_to_server.push((board_id.clone(), machine_id));
    }
    Ok(BoardPlacement { board_to_server })
}

/// True when any `(from, to)` board pair lands on different backends.
///
/// Missing board ids in `board_server` also return true (fail closed).
#[must_use]
pub fn has_cross_host_edge(
    edges: &[(String, String)],
    board_server: &HashMap<String, MachineId>,
) -> bool {
    edges
        .iter()
        .any(|(a, b)| match (board_server.get(a), board_server.get(b)) {
            (Some(&sa), Some(&sb)) => sa != sb,
            _ => true,
        })
}

/// True when any **Digital** topology edge spans different backends.
///
/// TODO(cluster-distributed-p2): provisional until Digital board-edge IPC exists.
/// Missing board ids fail closed (treated as cross-host).
#[must_use]
pub fn has_cross_host_digital_edge(
    edges: &[DirectedEdge],
    board_server: &HashMap<String, MachineId>,
) -> bool {
    edges.iter().any(|edge| {
        if edge.payload != PayloadKind::Digital {
            return false;
        }
        match (
            board_server.get(&edge.from.path),
            board_server.get(&edge.to.path),
        ) {
            (Some(&sa), Some(&sb)) => sa != sb,
            _ => true,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_bad_magic_and_short() {
        assert_eq!(MulticastMessage::decode(b"XXXX\x01\x01"), None);
        let mut short = MAGIC.to_vec();
        short.push(VERSION);
        short.push(MulticastMsgKind::VtMin as u8);
        short.extend_from_slice(&1u32.to_be_bytes());
        // missing min_vt_ns
        assert_eq!(MulticastMessage::decode(&short), None);
    }

    #[test]
    fn vt_min_round_trip() {
        let msg = MulticastMessage::VtMin {
            machine_id: MachineId::from_u32(0x0a00_0001),
            min_vt_ns: 12345,
        };
        let bytes = msg.encode();
        assert_eq!(MulticastMessage::decode(&bytes), Some(msg));
    }

    #[test]
    fn cluster_stop_round_trip() {
        let msg = MulticastMessage::ClusterStop {
            machine_id: MachineId::from_u32(1),
            reason: HostStopReason::Until,
        };
        assert_eq!(MulticastMessage::decode(&msg.encode()), Some(msg));
    }

    #[test]
    fn endpoint_round_trip_each_kind() {
        let kinds = [
            (
                ENDPOINT_KIND_UART,
                vec![0u8; std::mem::size_of::<sim_kernel::UartFrame>()],
            ),
            (
                ENDPOINT_KIND_ANALOG,
                vec![0u8; std::mem::size_of::<sim_kernel::AnalogVoltage>()],
            ),
            (
                ENDPOINT_KIND_PULSE,
                vec![0u8; std::mem::size_of::<sim_kernel::Pulse>()],
            ),
            (
                ENDPOINT_KIND_SCALAR,
                vec![0u8; std::mem::size_of::<sim_kernel::Scalar>()],
            ),
        ];
        for (payload_kind, payload) in kinds {
            let msg = MulticastMessage::Endpoint {
                machine_id: MachineId::from_u32(0x0a00_0002),
                edge_id: 0x1122_3344_5566_7788,
                payload_kind,
                payload,
            };
            assert_eq!(MulticastMessage::decode(&msg.encode()), Some(msg));
        }
    }

    #[test]
    fn endpoint_rejects_wrong_payload_len_and_kind() {
        let mut bad = MulticastMessage::Endpoint {
            machine_id: MachineId::from_u32(1),
            edge_id: 1,
            payload_kind: ENDPOINT_KIND_PULSE,
            payload: vec![0u8; 3],
        }
        .encode();
        assert_eq!(MulticastMessage::decode(&bad), None);
        // Truncate after payload_kind.
        bad.truncate(4 + 1 + 1 + 4 + 8 + 1);
        assert_eq!(MulticastMessage::decode(&bad), None);
        let unknown = MulticastMessage::Endpoint {
            machine_id: MachineId::from_u32(1),
            edge_id: 1,
            payload_kind: 99,
            payload: vec![0u8; 8],
        };
        assert_eq!(MulticastMessage::decode(&unknown.encode()), None);
    }

    #[test]
    fn cross_host_digital_edge_detect() {
        use crate::topology::{DirectedEdge, PortRef};

        let mut map = HashMap::new();
        map.insert("a".into(), MachineId::from_u32(10));
        map.insert("b".into(), MachineId::from_u32(20));
        let digital = DirectedEdge {
            from: PortRef {
                path: "a".into(),
                port: "d".into(),
            },
            to: PortRef {
                path: "b".into(),
                port: "d".into(),
            },
            route: vec![],
            payload: PayloadKind::Digital,
            uart_ring_len: None,
        };
        let uart = DirectedEdge {
            from: PortRef {
                path: "a".into(),
                port: "u".into(),
            },
            to: PortRef {
                path: "b".into(),
                port: "u".into(),
            },
            route: vec![],
            payload: PayloadKind::Uart,
            uart_ring_len: None,
        };
        assert!(has_cross_host_digital_edge(
            std::slice::from_ref(&digital),
            &map
        ));
        assert!(!has_cross_host_digital_edge(&[uart], &map));
        assert!(has_cross_host_digital_edge(
            &[DirectedEdge {
                to: PortRef {
                    path: "missing".into(),
                    port: "d".into(),
                },
                ..digital
            }],
            &map
        ));
    }

    #[test]
    fn uart_payload_rejects_invalid_enum() {
        let mut bad = encode_uart_payload(&sim_kernel::UartFrame::default());
        bad[16] = 9; // invalid stop_bits
        assert!(decode_uart_payload(&bad).is_none());
        assert!(!validate_endpoint_payload(ENDPOINT_KIND_UART, &bad));
    }

    #[test]
    fn remote_table_excludes_empty_as_max() {
        let mut t = RemoteVtTable::new();
        assert_eq!(t.aggregate_min(), u64::MAX);
        assert_eq!(t.upsert(MachineId::from_u32(1), 100), 100);
        assert_eq!(t.upsert(MachineId::from_u32(2), 50), 50);
        assert_eq!(t.upsert(MachineId::from_u32(1), 80), 50);
    }

    #[test]
    fn throttle_vt_delta_and_interval() {
        let mut th = VtPublishThrottle::new(1_000).with_interval(Duration::from_millis(100));
        let t0 = Instant::now();
        assert!(th.should_send(0, t0));
        th.record_send(0, t0);
        assert!(!th.should_send(100, t0)); // delta 100 < 500
        assert!(th.should_send(500, t0)); // delta == margin/2
        assert!(th.should_send(0, t0 + Duration::from_millis(100)));
    }

    #[test]
    fn allocate_skips_used() {
        let addrs = [(Ipv4Addr::new(239, 0, 0, 1), Ipv4Addr::new(239, 0, 0, 2))];
        let ports = [(30000, 30001)];
        let used = [(Ipv4Addr::new(239, 0, 0, 1), 30000)];
        assert_eq!(
            allocate_multicast(&addrs, &ports, &used),
            Some((Ipv4Addr::new(239, 0, 0, 1), 30001))
        );
    }

    #[test]
    fn first_fit_placement() {
        let boards = vec!["a".into(), "b".into(), "c".into()];
        let mid10 = MachineId::from_u32(10);
        let mid20 = MachineId::from_u32(20);
        let rem = HashMap::from([(mid10, 2u32), (mid20, 2u32)]);
        let place = place_boards_first_fit(&boards, &rem).unwrap();
        assert_eq!(place.board_to_server.len(), 3);
        // Input capacity map is unchanged.
        assert_eq!(rem, HashMap::from([(mid10, 2u32), (mid20, 2u32)]));
        for (board, mid) in &place.board_to_server {
            assert!(*mid == mid10 || *mid == mid20, "{board} -> {mid}");
        }
        // First-Fit packs into the first HashMap::iter entry until full.
        let first_mid = place.board_to_server[0].1;
        assert_eq!(place.board_to_server[0].1, first_mid);
        assert_eq!(place.board_to_server[1].1, first_mid);
        assert_ne!(place.board_to_server[2].1, first_mid);
        let packed_into_first = place
            .board_to_server
            .iter()
            .filter(|(_, mid)| *mid == first_mid)
            .count();
        assert_eq!(packed_into_first, 2);
    }

    #[test]
    fn cross_host_edge_detect() {
        let mut map = HashMap::new();
        map.insert("a".into(), MachineId::from_u32(10));
        map.insert("b".into(), MachineId::from_u32(20));
        assert!(has_cross_host_edge(&[("a".into(), "b".into())], &map));
        assert!(!has_cross_host_edge(&[("a".into(), "a".into())], &map));
        // Incomplete placement map must not be treated as same-host.
        assert!(has_cross_host_edge(&[("a".into(), "missing".into())], &map));
    }
}
