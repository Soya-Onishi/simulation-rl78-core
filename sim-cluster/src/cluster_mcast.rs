//! Phase-1 UDP multicast between arbiters: virtual-time mins and Cluster Stop.
//!
//! Wire format is fixed binary (not JSON). Endpoint payloads arrive in phase 2.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::control::HostStopReason;

/// Magic bytes for sim-cluster multicast datagrams.
const MAGIC: [u8; 4] = *b"SCMC";
const VERSION: u8 = 1;

/// Default keepalive / resend interval (wall clock), per design.
pub const DEFAULT_MAX_INTERVAL: Duration = Duration::from_millis(100);

/// Multicast message kinds (phase 1). Extended in phase 2+.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MulticastMsgKind {
    VtMin = 1,
    ClusterStop = 2,
}

impl MulticastMsgKind {
    #[must_use]
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::VtMin),
            2 => Some(Self::ClusterStop),
            _ => None,
        }
    }
}

/// Decoded multicast payload (phase 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MulticastMessage {
    /// Sender's local `min_board_vt`.
    VtMin {
        machine_id: u32,
        min_vt_ns: u64,
    },
    ClusterStop {
        machine_id: u32,
        reason: HostStopReason,
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
            Self::ClusterStop {
                machine_id,
                reason,
            } => {
                out.push(MulticastMsgKind::ClusterStop as u8);
                out.extend_from_slice(&machine_id.to_be_bytes());
                out.push(reason.to_wire());
            }
        }
        out
    }

    /// `None` if the datagram is not a well-formed phase-1 message.
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
                let machine_id = u32::from_be_bytes(rest[0..4].try_into().ok()?);
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
                let machine_id = u32::from_be_bytes(rest[0..4].try_into().ok()?);
                let reason = HostStopReason::from_wire(rest[4])?;
                Some(Self::ClusterStop {
                    machine_id,
                    reason,
                })
            }
        }
    }
}

/// Joined UDP multicast group for one simulation.
pub struct ClusterMulticast {
    socket: UdpSocket,
    group: SocketAddrV4,
    machine_id: u32,
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
        machine_id: u32,
        interface: Ipv4Addr,
    ) -> io::Result<Self> {
        // Bind the multicast address+port (common Linux pattern) so membership
        // delivers datagrams to this socket; SO_REUSEADDR allows multiple joins.
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
    pub fn machine_id(&self) -> u32 {
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
        let mut buf = [0u8; 64];
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
    peers: HashMap<u32, u64>,
}

impl RemoteVtTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update one remote; returns the new aggregate min for slot 0 (`u64::MAX` if empty).
    pub fn upsert(&mut self, machine_id: u32, min_vt_ns: u64) -> u64 {
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
                    if !used.iter().any(|&(u_addr, u_port)| u_addr == addr && u_port == port)
                    {
                        return Some((addr, port));
                    }
                }
            }
        }
    }
    None
}

/// First-Fit board placement onto servers (topology board order × servers order).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardPlacement {
    /// `board_id` → index into the frontend `servers` list.
    pub board_to_server: Vec<(String, usize)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlacementError {
    InsufficientCapacity {
        boards: usize,
        free_slots: u32,
    },
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

/// Place `board_ids` with First-Fit using remaining capacity per server.
pub fn place_boards_first_fit(
    board_ids: &[String],
    remaining: &mut [u32],
) -> Result<BoardPlacement, PlacementError> {
    let free_slots: u32 = remaining.iter().copied().sum();
    if (board_ids.len() as u32) > free_slots {
        return Err(PlacementError::InsufficientCapacity {
            boards: board_ids.len(),
            free_slots,
        });
    }
    let mut board_to_server = Vec::with_capacity(board_ids.len());
    let mut server = 0usize;
    for board_id in board_ids {
        while server < remaining.len() && remaining[server] == 0 {
            server += 1;
        }
        debug_assert!(server < remaining.len());
        remaining[server] -= 1;
        board_to_server.push((board_id.clone(), server));
    }
    Ok(BoardPlacement { board_to_server })
}

/// True when any topology edge connects boards on different servers.
///
/// TODO(cluster-distributed-p2): provisional — remove once cross-host endpoint
/// multicast (`node→arbiter→multicast→arbiter→node`) is implemented. See
/// `docs/cluster-distributed-p1.md`.
#[must_use]
pub fn has_cross_host_edge(
    edges: &[(String, String)],
    board_server: &HashMap<String, usize>,
) -> bool {
    edges.iter().any(|(a, b)| {
        match (board_server.get(a), board_server.get(b)) {
            (Some(&sa), Some(&sb)) => sa != sb,
            _ => false,
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
            machine_id: 0x0a00_0001,
            min_vt_ns: 12345,
        };
        let bytes = msg.encode();
        assert_eq!(MulticastMessage::decode(&bytes), Some(msg));
    }

    #[test]
    fn cluster_stop_round_trip() {
        let msg = MulticastMessage::ClusterStop {
            machine_id: 1,
            reason: HostStopReason::Until,
        };
        assert_eq!(MulticastMessage::decode(&msg.encode()), Some(msg));
    }

    #[test]
    fn remote_table_excludes_empty_as_max() {
        let mut t = RemoteVtTable::new();
        assert_eq!(t.aggregate_min(), u64::MAX);
        assert_eq!(t.upsert(1, 100), 100);
        assert_eq!(t.upsert(2, 50), 50);
        assert_eq!(t.upsert(1, 80), 50);
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
        let mut rem = [2, 2];
        let place = place_boards_first_fit(&boards, &mut rem).unwrap();
        assert_eq!(
            place.board_to_server,
            vec![("a".into(), 0), ("b".into(), 0), ("c".into(), 1)]
        );
        assert_eq!(rem, [0, 1]);
    }

    #[test]
    fn cross_host_edge_detect() {
        let mut map = HashMap::new();
        map.insert("a".into(), 0);
        map.insert("b".into(), 1);
        assert!(has_cross_host_edge(&[("a".into(), "b".into())], &map));
        assert!(!has_cross_host_edge(&[("a".into(), "a".into())], &map));
    }

}
