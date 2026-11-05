//! Arbiter time sync over IPv4 UDP multicast.
//!
//! Every arbiter of a multi-backend simulation periodically multicasts the
//! minimum virtual time of its own nodes ([`TimeReportPacket`]). Each arbiter
//! folds the packets of the other arbiters into [`TimeSyncState`], which gives
//! the minimum over all remote arbiters (`remote_min_ns`). The caller combines
//! that with its local minimum and adds the margin (`allowed = min + margin`,
//! see `TimeCeiling`).
//!
//! - A report is sent every `period` even when the value did not change
//!   (keepalive).
//! - A remote arbiter that has been silent for more than [`PEER_SILENCE`]
//!   freezes Allowed updates (the last Allowed stays in force). Nothing is
//!   stopped; updates resume as soon as packets return.
//! - A packet whose `cluster_key` differs from the local one, whose sender is
//!   not a listed peer, or that this arbiter sent itself is discarded.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::orchestrate::{ArbiterEndpoints, MulticastGroup};

/// A remote arbiter silent for longer than this freezes Allowed updates.
pub const PEER_SILENCE: Duration = Duration::from_secs(1);

const MAGIC: [u8; 4] = *b"RLTS";
const VERSION: u8 = 1;
/// Largest datagram this module sends or reads (header + two short strings + time).
const MAX_PACKET: usize = 4 + 1 + 1 + 255 + 1 + 255 + 8;

/// One arbiter's time report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeReportPacket {
    /// Shared cluster identity (`Phase2ConnectRequest::cluster_key`), the same
    /// in every arbiter of the simulation; not a member-local simulation id.
    pub cluster_key: String,
    /// Sending backend, as in `ArbiterEndpoints::backend`.
    pub sender: String,
    /// Minimum virtual time (ns) over the sender's local nodes.
    pub virtual_time_ns: u64,
}

impl TimeReportPacket {
    /// Wire layout: `"RLTS"`, version, `len(cluster_key)`, `cluster_key`,
    /// `len(sender)`, `sender`, `virtual_time_ns` (u64 little endian).
    /// `None` when a string is longer than 255 bytes.
    #[must_use]
    pub fn encode(&self) -> Option<Vec<u8>> {
        let key_len = u8::try_from(self.cluster_key.len()).ok()?;
        let sender_len = u8::try_from(self.sender.len()).ok()?;
        let mut out =
            Vec::with_capacity(MAGIC.len() + 3 + self.cluster_key.len() + self.sender.len() + 8);
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.push(key_len);
        out.extend_from_slice(self.cluster_key.as_bytes());
        out.push(sender_len);
        out.extend_from_slice(self.sender.as_bytes());
        out.extend_from_slice(&self.virtual_time_ns.to_le_bytes());
        Some(out)
    }

    /// `None` for anything that is not a well-formed version-1 report.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(&MAGIC)?;
        let (&version, rest) = rest.split_first()?;
        if version != VERSION {
            return None;
        }
        let (cluster_key, rest) = take_string(rest)?;
        let (sender, rest) = take_string(rest)?;
        let time: [u8; 8] = rest.try_into().ok()?;
        Some(Self {
            cluster_key,
            sender,
            virtual_time_ns: u64::from_le_bytes(time),
        })
    }
}

fn take_string(bytes: &[u8]) -> Option<(String, &[u8])> {
    let (&len, rest) = bytes.split_first()?;
    let len = usize::from(len);
    if rest.len() < len {
        return None;
    }
    let (text, rest) = rest.split_at(len);
    Some((String::from_utf8(text.to_vec()).ok()?, rest))
}

/// What the remote arbiters say right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteTimes {
    /// Minimum of the last virtual time reported by each remote arbiter
    /// (`0` for one that has not reported yet).
    pub remote_min_ns: u64,
    /// `true` while at least one remote arbiter has been silent for more than
    /// the silence limit.
    pub frozen: bool,
}

#[derive(Clone, Copy, Debug)]
struct PeerSlot {
    virtual_time_ns: u64,
    last_seen: Instant,
}

/// Socket-free time-sync bookkeeping (send schedule, report aggregation,
/// silence detection). All time comes in as arguments so it is testable.
#[derive(Debug)]
pub struct TimeSyncState {
    cluster_key: String,
    self_id: String,
    period: Duration,
    silence: Duration,
    peers: HashMap<String, PeerSlot>,
    last_sent: Option<Instant>,
}

impl TimeSyncState {
    /// `peers` are the other arbiters' backend keys. Their silence is counted
    /// from `now` until their first report.
    #[must_use]
    pub fn new(
        cluster_key: impl Into<String>,
        self_id: impl Into<String>,
        peers: impl IntoIterator<Item = String>,
        period: Duration,
        silence: Duration,
        now: Instant,
    ) -> Self {
        let peers = peers
            .into_iter()
            .map(|id| {
                (
                    id,
                    PeerSlot {
                        virtual_time_ns: 0,
                        last_seen: now,
                    },
                )
            })
            .collect();
        Self {
            cluster_key: cluster_key.into(),
            self_id: self_id.into(),
            period,
            silence,
            peers,
            last_sent: None,
        }
    }

    /// Record a received report. Returns `false` (and ignores it) when it
    /// belongs to another cluster, comes from this arbiter, or from a sender
    /// that is not a listed peer.
    pub fn accept(&mut self, packet: &TimeReportPacket, now: Instant) -> bool {
        if packet.cluster_key != self.cluster_key || packet.sender == self.self_id {
            return false;
        }
        let Some(slot) = self.peers.get_mut(&packet.sender) else {
            return false;
        };
        slot.virtual_time_ns = packet.virtual_time_ns;
        slot.last_seen = now;
        true
    }

    /// `true` before the first send and then once every period.
    #[must_use]
    pub fn send_due(&self, now: Instant) -> bool {
        self.last_sent
            .is_none_or(|last| now.saturating_duration_since(last) >= self.period)
    }

    pub fn mark_sent(&mut self, now: Instant) {
        self.last_sent = Some(now);
    }

    /// Packet to multicast with the given local minimum.
    #[must_use]
    pub fn report(&self, local_min_ns: u64) -> TimeReportPacket {
        TimeReportPacket {
            cluster_key: self.cluster_key.clone(),
            sender: self.self_id.clone(),
            virtual_time_ns: local_min_ns,
        }
    }

    #[must_use]
    pub fn remote_times(&self, now: Instant) -> RemoteTimes {
        let remote_min_ns = self
            .peers
            .values()
            .map(|slot| slot.virtual_time_ns)
            .min()
            .unwrap_or(0);
        let frozen = self
            .peers
            .values()
            .any(|slot| now.saturating_duration_since(slot.last_seen) > self.silence);
        RemoteTimes {
            remote_min_ns,
            frozen,
        }
    }
}

/// Failed to set up the time-sync multicast socket.
#[derive(Debug, thiserror::Error)]
pub enum TimeMcastError {
    #[error("invalid multicast group `{0}:{1}`: expected an IPv4 multicast address and port")]
    BadGroup(String, u16),
    #[error("invalid advertise_ip `{0}`: expected IPv4")]
    BadInterface(String),
    #[error("time-sync multicast socket: {0}")]
    Socket(#[from] std::io::Error),
}

/// Multicast socket plus [`TimeSyncState`] for one simulation's arbiter.
pub struct TimeMcast {
    socket: UdpSocket,
    group: SocketAddrV4,
    state: TimeSyncState,
    buf: Vec<u8>,
}

impl TimeMcast {
    /// Join `group` on the interface that owns `advertise_ip` and start
    /// reporting. `peers` are the other arbiters of the simulation.
    ///
    /// `cluster_key` must be the shared `Phase2ConnectRequest::cluster_key`
    /// (identical in every arbiter), not the member-local iceoryx id.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        cluster_key: &str,
        self_id: &str,
        advertise_ip: &str,
        group: &MulticastGroup,
        peers: &[ArbiterEndpoints],
        period: Duration,
        now: Instant,
    ) -> Result<Self, TimeMcastError> {
        let group_ip: Ipv4Addr = group
            .ip
            .parse()
            .ok()
            .filter(|ip: &Ipv4Addr| ip.is_multicast() && group.port != 0)
            .ok_or_else(|| TimeMcastError::BadGroup(group.ip.clone(), group.port))?;
        let iface: Ipv4Addr = advertise_ip
            .parse()
            .map_err(|_| TimeMcastError::BadInterface(advertise_ip.to_string()))?;

        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        // Several arbiters of one simulation may share a host.
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        // Binding to the group address keeps other groups on the same port out.
        #[cfg(unix)]
        let bind_ip = group_ip;
        #[cfg(not(unix))]
        let bind_ip = Ipv4Addr::UNSPECIFIED;
        socket.bind(&SockAddr::from(SocketAddrV4::new(bind_ip, group.port)))?;
        socket.join_multicast_v4(&group_ip, &iface)?;
        socket.set_multicast_if_v4(&iface)?;
        // Same L2 segment only; other arbiters on this host must hear us too.
        socket.set_multicast_ttl_v4(1)?;
        socket.set_multicast_loop_v4(true)?;
        socket.set_nonblocking(true)?;

        Ok(Self {
            socket: UdpSocket::from(socket),
            group: SocketAddrV4::new(group_ip, group.port),
            state: TimeSyncState::new(
                cluster_key,
                self_id,
                peers.iter().map(|p| p.backend.clone()),
                period,
                PEER_SILENCE,
                now,
            ),
            buf: vec![0u8; MAX_PACKET],
        })
    }

    /// Drain received reports, multicast ours when a period elapsed, and return
    /// the remote view. `local_min_ns` is the minimum virtual time of this
    /// arbiter's own nodes.
    pub fn poll(&mut self, local_min_ns: u64, now: Instant) -> RemoteTimes {
        loop {
            let len = match self.socket.recv_from(&mut self.buf) {
                Ok((len, _)) => len,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    log::warn!("time-sync multicast receive: {err}");
                    break;
                }
            };
            if let Some(packet) = TimeReportPacket::decode(&self.buf[..len]) {
                self.state.accept(&packet, now);
            }
        }
        if self.state.send_due(now) {
            if let Some(bytes) = self.state.report(local_min_ns).encode() {
                if let Err(err) = self.socket.send_to(&bytes, self.group) {
                    log::warn!("time-sync multicast send: {err}");
                }
            }
            // Count the attempt so a failing socket does not spin on send.
            self.state.mark_sent(now);
        }
        self.state.remote_times(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(peers: &[&str], now: Instant) -> TimeSyncState {
        TimeSyncState::new(
            "sim-1",
            "10.0.0.1:8091",
            peers.iter().map(|p| (*p).to_string()),
            Duration::from_millis(100),
            PEER_SILENCE,
            now,
        )
    }

    fn report(key: &str, sender: &str, vt: u64) -> TimeReportPacket {
        TimeReportPacket {
            cluster_key: key.into(),
            sender: sender.into(),
            virtual_time_ns: vt,
        }
    }

    #[test]
    fn packet_round_trips_and_rejects_garbage() {
        let pkt = report("sim-1", "10.0.0.2:8091", 123_456_789);
        let bytes = pkt.encode().unwrap();
        assert_eq!(TimeReportPacket::decode(&bytes), Some(pkt));
        assert_eq!(TimeReportPacket::decode(b""), None);
        assert_eq!(TimeReportPacket::decode(b"XXXX\x01"), None);
        assert_eq!(TimeReportPacket::decode(&bytes[..bytes.len() - 1]), None);
        let mut long = bytes.clone();
        long.push(0);
        assert_eq!(TimeReportPacket::decode(&long), None);
        let mut other_version = bytes;
        other_version[4] = 2;
        assert_eq!(TimeReportPacket::decode(&other_version), None);
        assert!(report(&"k".repeat(256), "s", 0).encode().is_none());
    }

    #[test]
    fn remote_min_is_minimum_over_peers_and_zero_before_first_report() {
        let t0 = Instant::now();
        let mut s = state(&["b:1", "c:1"], t0);
        assert_eq!(s.remote_times(t0).remote_min_ns, 0);
        assert!(s.accept(&report("sim-1", "b:1", 500), t0));
        // `c` has not reported yet, so it still pins the minimum at 0.
        assert_eq!(s.remote_times(t0).remote_min_ns, 0);
        assert!(s.accept(&report("sim-1", "c:1", 300), t0));
        assert_eq!(s.remote_times(t0).remote_min_ns, 300);
        assert!(s.accept(&report("sim-1", "c:1", 900), t0));
        assert_eq!(s.remote_times(t0).remote_min_ns, 500);
    }

    #[test]
    fn allowed_is_min_of_local_and_remote_plus_margin() {
        use crate::time_sync::TimeCeiling;
        let t0 = Instant::now();
        let mut s = state(&["b:1"], t0);
        let mut ceil = TimeCeiling::new(["a".into()], 1000);
        ceil.report("a", 700);
        s.accept(&report("sim-1", "b:1", 400), t0);
        let remote = s.remote_times(t0);
        ceil.set_remote(Some(remote.remote_min_ns), remote.frozen);
        assert_eq!(ceil.allowed_ns(), 1400);
        s.accept(&report("sim-1", "b:1", 2000), t0);
        let remote = s.remote_times(t0);
        ceil.set_remote(Some(remote.remote_min_ns), remote.frozen);
        assert_eq!(ceil.allowed_ns(), 1700);
    }

    #[test]
    fn silent_peer_freezes_after_one_second_and_resumes_on_packet() {
        let t0 = Instant::now();
        let mut s = state(&["b:1", "c:1"], t0);
        s.accept(&report("sim-1", "b:1", 10), t0);
        s.accept(&report("sim-1", "c:1", 20), t0);
        assert!(!s.remote_times(t0 + Duration::from_millis(1000)).frozen);
        assert!(s.remote_times(t0 + Duration::from_millis(1001)).frozen);

        // Only `b` returns: `c` is still silent, so the freeze holds and the
        // last value of `c` is kept.
        let t1 = t0 + Duration::from_millis(1500);
        s.accept(&report("sim-1", "b:1", 50), t1);
        let held = s.remote_times(t1);
        assert!(held.frozen);
        assert_eq!(held.remote_min_ns, 20);

        s.accept(&report("sim-1", "c:1", 60), t1);
        let back = s.remote_times(t1);
        assert!(!back.frozen);
        assert_eq!(back.remote_min_ns, 50);
    }

    #[test]
    fn never_heard_peer_counts_silence_from_creation() {
        let t0 = Instant::now();
        let s = state(&["b:1"], t0);
        assert!(!s.remote_times(t0 + Duration::from_millis(900)).frozen);
        assert!(s.remote_times(t0 + Duration::from_millis(1100)).frozen);
    }

    #[test]
    fn frozen_ceiling_keeps_flag_and_dirty_for_resume() {
        use crate::time_sync::TimeCeiling;
        let mut ceil = TimeCeiling::new(["a".into()], 1000);
        ceil.set_remote(Some(5), true);
        assert!(ceil.is_frozen());
        assert!(ceil.take_remote_dirty());
        assert!(!ceil.take_remote_dirty());
        ceil.set_remote(Some(5), true);
        assert!(!ceil.take_remote_dirty());
        ceil.set_remote(Some(5), false);
        assert!(!ceil.is_frozen());
        assert!(ceil.take_remote_dirty());
    }

    #[test]
    fn accept_filters_cluster_key_self_and_unknown_sender() {
        let t0 = Instant::now();
        let mut s = state(&["b:1"], t0);
        assert!(!s.accept(&report("other-sim", "b:1", 99), t0));
        assert!(!s.accept(&report("sim-1", "10.0.0.1:8091", 99), t0));
        assert!(!s.accept(&report("sim-1", "z:1", 99), t0));
        assert_eq!(s.remote_times(t0).remote_min_ns, 0);
        // A mismatching packet does not refresh liveness either.
        let late = t0 + Duration::from_millis(1500);
        assert!(!s.accept(&report("other-sim", "b:1", 99), late));
        assert!(s.remote_times(late).frozen);
        assert!(s.accept(&report("sim-1", "b:1", 7), late));
        assert_eq!(s.remote_times(late).remote_min_ns, 7);
    }

    fn state_of(cluster_key: &str, self_id: &str, peer: &str, now: Instant) -> TimeSyncState {
        TimeSyncState::new(
            cluster_key,
            self_id,
            [peer.to_string()],
            Duration::from_millis(100),
            PEER_SILENCE,
            now,
        )
    }

    #[test]
    fn arbiters_sharing_a_cluster_key_accept_each_other_whatever_their_member_ids() {
        // Each arbiter has its own member-local simulation id ("sim-0", "sim-1"),
        // but both are configured with the one shared cluster key "sim".
        let t0 = Instant::now();
        let mut a = state_of("sim", "a:1", "b:1", t0);
        let mut b = state_of("sim", "b:1", "a:1", t0);
        let late = t0 + Duration::from_millis(1500);

        assert!(b.accept(&a.report(400), late));
        assert!(a.accept(&b.report(900), late));
        assert_eq!(a.remote_times(late).remote_min_ns, 900);
        assert_eq!(b.remote_times(late).remote_min_ns, 400);
        assert!(!a.remote_times(late).frozen);
        assert!(!b.remote_times(late).frozen);
    }

    #[test]
    fn arbiters_with_different_cluster_keys_discard_each_other() {
        let t0 = Instant::now();
        let mut a = state_of("sim-0", "a:1", "b:1", t0);
        let mut b = state_of("sim-1", "b:1", "a:1", t0);
        let late = t0 + Duration::from_millis(1500);

        assert!(!b.accept(&a.report(400), late));
        assert!(!a.accept(&b.report(900), late));
        assert_eq!(a.remote_times(late).remote_min_ns, 0);
        assert!(a.remote_times(late).frozen);
        assert!(b.remote_times(late).frozen);
    }

    #[test]
    fn send_is_due_first_then_every_period_even_when_unchanged() {
        let t0 = Instant::now();
        let mut s = state(&["b:1"], t0);
        assert!(s.send_due(t0));
        s.mark_sent(t0);
        assert!(!s.send_due(t0 + Duration::from_millis(99)));
        assert!(s.send_due(t0 + Duration::from_millis(100)));
        s.mark_sent(t0 + Duration::from_millis(100));
        assert!(!s.send_due(t0 + Duration::from_millis(150)));
        assert!(s.send_due(t0 + Duration::from_millis(200)));
        // The packet is the same for an unchanged local time.
        assert_eq!(s.report(42), s.report(42));
        assert_eq!(s.report(42).virtual_time_ns, 42);
        assert_eq!(s.report(42).sender, "10.0.0.1:8091");
    }

    fn endpoints(backend: &str) -> ArbiterEndpoints {
        ArbiterEndpoints {
            backend: backend.into(),
            advertise_ip: "127.0.0.1".into(),
            control_tcp_port: 1,
            data_udp_port: 2,
        }
    }

    #[test]
    fn open_rejects_bad_group_and_interface() {
        let now = Instant::now();
        let bad_group = MulticastGroup {
            ip: "10.0.0.1".into(),
            port: 47000,
        };
        assert!(matches!(
            TimeMcast::open(
                "k",
                "a:1",
                "127.0.0.1",
                &bad_group,
                &[],
                Duration::from_millis(10),
                now
            ),
            Err(TimeMcastError::BadGroup(..))
        ));
        let group = MulticastGroup {
            ip: "239.255.77.1".into(),
            port: 47001,
        };
        assert!(matches!(
            TimeMcast::open(
                "k",
                "a:1",
                "nope",
                &group,
                &[],
                Duration::from_millis(10),
                now
            ),
            Err(TimeMcastError::BadInterface(_))
        ));
    }

    /// Two arbiters on one host exchange reports over a real multicast group.
    /// Skipped when the host cannot join multicast on loopback.
    #[test]
    fn two_arbiters_exchange_reports_over_loopback_multicast() {
        let group = MulticastGroup {
            ip: "239.255.77.2".into(),
            port: 47002,
        };
        let a_ep = endpoints("a:1");
        let b_ep = endpoints("b:1");
        let period = Duration::from_millis(5);
        let now = Instant::now();
        let open = |key: &str, me: &str, peer: &ArbiterEndpoints| {
            TimeMcast::open(
                key,
                me,
                "127.0.0.1",
                &group,
                std::slice::from_ref(peer),
                period,
                now,
            )
        };
        let (Ok(mut a), Ok(mut b)) = (open("sim", "a:1", &b_ep), open("sim", "b:1", &a_ep)) else {
            eprintln!("skipping: multicast join on loopback unavailable");
            return;
        };
        // A third arbiter of another simulation shares the group; it must not
        // disturb the others.
        let Ok(mut stranger) = TimeMcast::open(
            "other",
            "a:1",
            "127.0.0.1",
            &group,
            std::slice::from_ref(&b_ep),
            period,
            now,
        ) else {
            return;
        };

        let deadline = Instant::now() + Duration::from_secs(2);
        let (mut seen_a, mut seen_b) = (None, None);
        while Instant::now() < deadline && (seen_a != Some(900) || seen_b != Some(400)) {
            let t = Instant::now();
            let ra = a.poll(400, t);
            let rb = b.poll(900, t);
            let _ = stranger.poll(1, t);
            seen_a = Some(ra.remote_min_ns);
            seen_b = Some(rb.remote_min_ns);
            std::thread::sleep(Duration::from_millis(2));
        }
        if seen_a == Some(0) && seen_b == Some(0) {
            eprintln!("skipping: no multicast delivery on loopback in this environment");
            return;
        }
        assert_eq!(seen_a, Some(900));
        assert_eq!(seen_b, Some(400));
    }
}
