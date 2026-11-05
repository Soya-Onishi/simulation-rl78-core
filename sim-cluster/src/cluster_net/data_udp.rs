//! Cross-machine data UDP: a sliding-window reliable message stream between arbiters.
//!
//! Path of a cross-machine UART/analog sample:
//! `Node → iceoryx2 → local arbiter → this link → peer arbiter → iceoryx2 → Node`.
//!
//! One [`PeerChannel`] exists per peer arbiter. All cross edges towards the same
//! peer share its message stream (one sequence space, one cumulative ACK).
//!
//! # Protocol
//!
//! * Every datagram starts with a header: magic `"DU"`, version, packet type, the
//!   shared cluster key (1-byte length + bytes) and the sender's backend key
//!   (1-byte length + bytes). Datagrams whose cluster key differs from ours, or
//!   whose sender is not a listed peer, are dropped.
//! * `DATA` carries a `u64` sequence number (big endian) and one opaque message.
//! * `ACK` carries the receiver's *next expected* sequence number (cumulative:
//!   everything below it has been delivered in order).
//! * The sender keeps at most `window` unacknowledged messages in flight and keeps
//!   sending new ones while earlier ones are still unacknowledged (not
//!   stop-and-wait). A message that stays unacknowledged for
//!   [`RETRANSMIT_INTERVAL`] (fixed 200 ms) is sent again.
//! * The receiver buffers out-of-order messages that fall inside its window,
//!   delivers messages strictly in sequence order, and answers every DATA packet
//!   with an ACK (also for duplicates, so a lost ACK is repaired).
//!
//! Plain IPv4 UDP, no authentication.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use sim_kernel::{AnalogVoltage, Tick, UartFrame, UartParity, UartStopBits};

use crate::orchestrate::ArbiterEndpoints;

/// Default for `BackendConfig.data_udp_window` (messages in flight per peer).
pub const DEFAULT_DATA_UDP_WINDOW: u32 = 32;

/// Fixed retransmit timeout for an unacknowledged message.
pub const RETRANSMIT_INTERVAL: Duration = Duration::from_millis(200);

/// Messages that may wait for a free window slot before the caller must stop
/// feeding the sender (see [`SlidingWindowSend::has_room`]).
pub const PENDING_LIMIT: usize = 4096;

const MAGIC: [u8; 2] = *b"DU";
const VERSION: u8 = 1;
const TYPE_DATA: u8 = 1;
const TYPE_ACK: u8 = 2;
/// Largest datagram this link reads. Messages are far smaller than any MTU.
const MAX_DATAGRAM: usize = 1500;
/// Datagrams read from the socket per [`DataLink::poll`] call.
const RECV_BUDGET: usize = 1024;

/// Failure to set up or use the data link.
#[derive(Debug, thiserror::Error)]
pub enum DataLinkError {
    #[error("peer {backend}: invalid advertise_ip `{ip}`")]
    BadPeerAddress { backend: String, ip: String },
    #[error("cluster key and backend key must each be 1..=255 bytes")]
    BadKeyLength,
    #[error("data window must be greater than 0")]
    ZeroWindow,
    #[error("unknown peer {0}")]
    UnknownPeer(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

// ---------------------------------------------------------------------------
// Sliding window (pure state machines, no I/O)
// ---------------------------------------------------------------------------

struct Inflight {
    seq: u64,
    payload: Vec<u8>,
    /// `None` until the first transmission.
    last_sent: Option<Instant>,
}

/// Sender half of the window protocol.
pub struct SlidingWindowSend {
    window: usize,
    retransmit: Duration,
    next_seq: u64,
    pending: VecDeque<Vec<u8>>,
    inflight: VecDeque<Inflight>,
    retransmits: u64,
}

impl SlidingWindowSend {
    #[must_use]
    pub fn new(window: u32, retransmit: Duration) -> Self {
        Self {
            window: window.max(1) as usize,
            retransmit,
            next_seq: 0,
            pending: VecDeque::new(),
            inflight: VecDeque::new(),
            retransmits: 0,
        }
    }

    /// `true` while [`push`](Self::push) still accepts messages.
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.pending.len() < PENDING_LIMIT
    }

    /// Queue `payload`. Returns `false` (message not queued) when the backlog is full.
    pub fn push(&mut self, payload: Vec<u8>) -> bool {
        if !self.has_room() {
            return false;
        }
        self.pending.push_back(payload);
        true
    }

    /// Messages sent but not yet acknowledged.
    #[must_use]
    pub fn inflight_len(&self) -> usize {
        self.inflight.len()
    }

    /// Messages waiting for a window slot.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Number of retransmissions so far.
    #[must_use]
    pub fn retransmits(&self) -> u64 {
        self.retransmits
    }

    /// Cumulative ACK: the receiver has delivered every message below `next`.
    /// An ACK beyond what was ever sent is ignored.
    pub fn on_ack(&mut self, next: u64) {
        if next > self.next_seq {
            return;
        }
        while self.inflight.front().is_some_and(|m| m.seq < next) {
            self.inflight.pop_front();
        }
    }

    /// Datagrams to put on the wire at `now`: new messages that fit into the
    /// window (without waiting for earlier ACKs) plus in-flight messages whose
    /// retransmit timer expired.
    pub fn poll_transmit(&mut self, now: Instant) -> Vec<(u64, Vec<u8>)> {
        while self.inflight.len() < self.window {
            let Some(payload) = self.pending.pop_front() else {
                break;
            };
            self.inflight.push_back(Inflight {
                seq: self.next_seq,
                payload,
                last_sent: None,
            });
            self.next_seq += 1;
        }
        let mut out = Vec::new();
        for msg in &mut self.inflight {
            let due = match msg.last_sent {
                None => true,
                Some(sent) => now.saturating_duration_since(sent) >= self.retransmit,
            };
            if !due {
                continue;
            }
            if msg.last_sent.is_some() {
                self.retransmits += 1;
            }
            msg.last_sent = Some(now);
            out.push((msg.seq, msg.payload.clone()));
        }
        out
    }
}

/// Result of feeding one DATA packet to [`SlidingWindowRecv::on_data`].
#[derive(Debug, PartialEq, Eq)]
pub struct RecvOutcome {
    /// Messages now deliverable, in sequence order.
    pub delivered: Vec<Vec<u8>>,
    /// Value for the ACK to send back (next expected sequence number).
    pub ack_next: u64,
}

/// Receiver half of the window protocol.
pub struct SlidingWindowRecv {
    window: u64,
    expected: u64,
    buffered: BTreeMap<u64, Vec<u8>>,
}

impl SlidingWindowRecv {
    #[must_use]
    pub fn new(window: u32) -> Self {
        Self {
            window: u64::from(window.max(1)),
            expected: 0,
            buffered: BTreeMap::new(),
        }
    }

    /// Next sequence number the receiver waits for.
    #[must_use]
    pub fn expected(&self) -> u64 {
        self.expected
    }

    pub fn on_data(&mut self, seq: u64, payload: Vec<u8>) -> RecvOutcome {
        let mut delivered = Vec::new();
        // Below `expected`: duplicate. At or past the window: not acceptable yet.
        // Both are only acknowledged; the sender retransmits what is still missing.
        if seq >= self.expected && seq < self.expected + self.window {
            self.buffered.entry(seq).or_insert(payload);
            while let Some(msg) = self.buffered.remove(&self.expected) {
                delivered.push(msg);
                self.expected += 1;
            }
        }
        RecvOutcome {
            delivered,
            ack_next: self.expected,
        }
    }
}

// ---------------------------------------------------------------------------
// Datagram framing
// ---------------------------------------------------------------------------

/// Decoded datagram (borrows from the receive buffer).
#[derive(Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Data {
        cluster_key: &'a str,
        sender: &'a str,
        seq: u64,
        payload: &'a [u8],
    },
    Ack {
        cluster_key: &'a str,
        sender: &'a str,
        next: u64,
    },
}

/// Why a datagram was rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    TooShort,
    BadMagic,
    BadVersion(u8),
    BadType(u8),
    BadText,
}

fn put_header(out: &mut Vec<u8>, kind: u8, cluster_key: &str, sender: &str) {
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(kind);
    out.push(cluster_key.len() as u8);
    out.extend_from_slice(cluster_key.as_bytes());
    out.push(sender.len() as u8);
    out.extend_from_slice(sender.as_bytes());
}

/// Encode a DATA datagram. Both keys must be 1..=255 bytes.
#[must_use]
pub fn encode_data(cluster_key: &str, sender: &str, seq: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + cluster_key.len() + sender.len() + 8 + payload.len());
    put_header(&mut out, TYPE_DATA, cluster_key, sender);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Encode an ACK datagram. Both keys must be 1..=255 bytes.
#[must_use]
pub fn encode_ack(cluster_key: &str, sender: &str, next: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + cluster_key.len() + sender.len() + 8);
    put_header(&mut out, TYPE_ACK, cluster_key, sender);
    out.extend_from_slice(&next.to_be_bytes());
    out
}

fn take<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8], DecodeError> {
    if buf.len() < n {
        return Err(DecodeError::TooShort);
    }
    let (head, tail) = buf.split_at(n);
    *buf = tail;
    Ok(head)
}

fn take_text<'a>(buf: &mut &'a [u8]) -> Result<&'a str, DecodeError> {
    let len = usize::from(take(buf, 1)?[0]);
    std::str::from_utf8(take(buf, len)?).map_err(|_| DecodeError::BadText)
}

fn take_u64(buf: &mut &[u8]) -> Result<u64, DecodeError> {
    let bytes: [u8; 8] = take(buf, 8)?
        .try_into()
        .map_err(|_| DecodeError::TooShort)?;
    Ok(u64::from_be_bytes(bytes))
}

/// Decode one datagram.
pub fn decode(datagram: &[u8]) -> Result<Packet<'_>, DecodeError> {
    let mut buf = datagram;
    if take(&mut buf, 2)? != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let version = take(&mut buf, 1)?[0];
    if version != VERSION {
        return Err(DecodeError::BadVersion(version));
    }
    let kind = take(&mut buf, 1)?[0];
    let cluster_key = take_text(&mut buf)?;
    let sender = take_text(&mut buf)?;
    match kind {
        TYPE_DATA => {
            let seq = take_u64(&mut buf)?;
            Ok(Packet::Data {
                cluster_key,
                sender,
                seq,
                payload: buf,
            })
        }
        TYPE_ACK => {
            let next = take_u64(&mut buf)?;
            Ok(Packet::Ack {
                cluster_key,
                sender,
                next,
            })
        }
        other => Err(DecodeError::BadType(other)),
    }
}

// ---------------------------------------------------------------------------
// Edge message (the opaque payload carried in DATA)
// ---------------------------------------------------------------------------

/// Sample carried over a cross-machine edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeValue {
    Uart(UartFrame),
    Analog(AnalogVoltage),
}

/// One edge sample: `edge_id` (see `ipc::edge_id`) selects the target service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeMessage {
    pub edge_id: String,
    pub value: EdgeValue,
}

const KIND_UART: u8 = 1;
const KIND_ANALOG: u8 = 2;

impl EdgeMessage {
    /// `edge_id` (u16 length + bytes), kind byte, then the fixed-size body.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let id = self.edge_id.as_bytes();
        let mut out = Vec::with_capacity(2 + id.len() + 1 + 16);
        out.extend_from_slice(&(id.len() as u16).to_be_bytes());
        out.extend_from_slice(id);
        match self.value {
            EdgeValue::Uart(f) => {
                out.push(KIND_UART);
                out.extend_from_slice(&f.data.to_be_bytes());
                out.push(f.data_bits);
                out.extend_from_slice(&f.bit_time.0.to_be_bytes());
                out.push(f.stop_bits as u8);
                out.push(f.parity as u8);
                out.push(u8::from(f.inverted));
            }
            EdgeValue::Analog(v) => {
                out.push(KIND_ANALOG);
                out.extend_from_slice(&v.0.to_be_bytes());
            }
        }
        out
    }

    /// `None` for a truncated or unknown message.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut buf = bytes;
        let len = usize::from(u16::from_be_bytes(take(&mut buf, 2).ok()?.try_into().ok()?));
        let edge_id = std::str::from_utf8(take(&mut buf, len).ok()?)
            .ok()?
            .to_owned();
        let kind = take(&mut buf, 1).ok()?[0];
        let value = match kind {
            KIND_UART => {
                let data = u16::from_be_bytes(take(&mut buf, 2).ok()?.try_into().ok()?);
                let data_bits = take(&mut buf, 1).ok()?[0];
                let bit_time = take_u64(&mut buf).ok()?;
                let stop_bits = match take(&mut buf, 1).ok()?[0] {
                    0 => UartStopBits::One,
                    1 => UartStopBits::Two,
                    _ => return None,
                };
                let parity = match take(&mut buf, 1).ok()?[0] {
                    0 => UartParity::None,
                    1 => UartParity::Even,
                    2 => UartParity::Odd,
                    _ => return None,
                };
                let inverted = take(&mut buf, 1).ok()?[0] != 0;
                EdgeValue::Uart(UartFrame {
                    data,
                    data_bits,
                    bit_time: Tick(bit_time),
                    stop_bits,
                    parity,
                    inverted,
                })
            }
            KIND_ANALOG => {
                let raw = take_u64(&mut buf).ok()?;
                EdgeValue::Analog(AnalogVoltage(raw as i64))
            }
            _ => return None,
        };
        buf.is_empty().then_some(Self { edge_id, value })
    }
}

// ---------------------------------------------------------------------------
// UDP link
// ---------------------------------------------------------------------------

struct PeerChannel {
    addr: SocketAddr,
    send: SlidingWindowSend,
    recv: SlidingWindowRecv,
}

/// Counters for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DataLinkStats {
    /// Datagrams dropped for a foreign cluster key, unknown sender or bad framing.
    pub dropped_foreign: u64,
}

/// Unicast data UDP link from this arbiter to every peer arbiter.
pub struct DataLink {
    socket: UdpSocket,
    cluster_key: String,
    local_backend: String,
    peers: HashMap<String, PeerChannel>,
    stats: DataLinkStats,
}

impl DataLink {
    /// `socket` must be non-blocking (the bound `PeerListeners::data_udp`, or a
    /// `try_clone` of it). `cluster_key` is the shared
    /// `Phase2ConnectRequest::cluster_key`; `local_backend` is this arbiter's own
    /// backend key, stamped on every datagram. `peers` are the other arbiters.
    pub fn new(
        socket: UdpSocket,
        cluster_key: &str,
        local_backend: &str,
        peers: &[ArbiterEndpoints],
        window: u32,
    ) -> Result<Self, DataLinkError> {
        if window == 0 {
            return Err(DataLinkError::ZeroWindow);
        }
        let key_ok = |s: &str| (1..=usize::from(u8::MAX)).contains(&s.len());
        if !key_ok(cluster_key) || !key_ok(local_backend) {
            return Err(DataLinkError::BadKeyLength);
        }
        let mut map = HashMap::new();
        for peer in peers {
            if !key_ok(&peer.backend) {
                return Err(DataLinkError::BadKeyLength);
            }
            let ip: Ipv4Addr =
                peer.advertise_ip
                    .parse()
                    .map_err(|_| DataLinkError::BadPeerAddress {
                        backend: peer.backend.clone(),
                        ip: peer.advertise_ip.clone(),
                    })?;
            map.insert(
                peer.backend.clone(),
                PeerChannel {
                    addr: SocketAddr::V4(SocketAddrV4::new(ip, peer.data_udp_port)),
                    send: SlidingWindowSend::new(window, RETRANSMIT_INTERVAL),
                    recv: SlidingWindowRecv::new(window),
                },
            );
        }
        Ok(Self {
            socket,
            cluster_key: cluster_key.to_owned(),
            local_backend: local_backend.to_owned(),
            peers: map,
            stats: DataLinkStats::default(),
        })
    }

    /// `true` when a message for `peer` can be queued right now.
    #[must_use]
    pub fn can_send(&self, peer: &str) -> bool {
        self.peers.get(peer).is_some_and(|p| p.send.has_room())
    }

    /// Queue `message` for `peer`; it goes out on the next [`poll`](Self::poll).
    pub fn send(&mut self, peer: &str, message: Vec<u8>) -> Result<bool, DataLinkError> {
        let channel = self
            .peers
            .get_mut(peer)
            .ok_or_else(|| DataLinkError::UnknownPeer(peer.to_owned()))?;
        Ok(channel.send.push(message))
    }

    #[must_use]
    pub fn stats(&self) -> DataLinkStats {
        self.stats
    }

    /// Messages in flight (sent, not acknowledged) towards `peer`.
    #[must_use]
    pub fn inflight(&self, peer: &str) -> usize {
        self.peers.get(peer).map_or(0, |p| p.send.inflight_len())
    }

    /// Receive datagrams, answer them, then (re)transmit what is due at `now`.
    /// Returns the messages delivered in order, tagged with the sending backend.
    pub fn poll(&mut self, now: Instant) -> Vec<(String, Vec<u8>)> {
        let mut delivered = Vec::new();
        let mut buf = [0u8; MAX_DATAGRAM];
        for _ in 0..RECV_BUDGET {
            let len = match self.socket.recv_from(&mut buf) {
                Ok((len, _)) => len,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    log::warn!("data udp recv: {err}");
                    break;
                }
            };
            let Ok(packet) = decode(&buf[..len]) else {
                self.stats.dropped_foreign += 1;
                continue;
            };
            match packet {
                Packet::Data {
                    cluster_key,
                    sender,
                    seq,
                    payload,
                } => {
                    if cluster_key != self.cluster_key {
                        self.stats.dropped_foreign += 1;
                        continue;
                    }
                    let Some(channel) = self.peers.get_mut(sender) else {
                        self.stats.dropped_foreign += 1;
                        continue;
                    };
                    let outcome = channel.recv.on_data(seq, payload.to_vec());
                    let addr = channel.addr;
                    let sender = sender.to_owned();
                    for msg in outcome.delivered {
                        delivered.push((sender.clone(), msg));
                    }
                    let ack = encode_ack(&self.cluster_key, &self.local_backend, outcome.ack_next);
                    Self::transmit(&self.socket, addr, &ack);
                }
                Packet::Ack {
                    cluster_key,
                    sender,
                    next,
                } => {
                    if cluster_key != self.cluster_key {
                        self.stats.dropped_foreign += 1;
                        continue;
                    }
                    match self.peers.get_mut(sender) {
                        Some(channel) => channel.send.on_ack(next),
                        None => self.stats.dropped_foreign += 1,
                    }
                }
            }
        }
        for channel in self.peers.values_mut() {
            for (seq, payload) in channel.send.poll_transmit(now) {
                let datagram = encode_data(&self.cluster_key, &self.local_backend, seq, &payload);
                Self::transmit(&self.socket, channel.addr, &datagram);
            }
        }
        delivered
    }

    /// Best effort: a datagram that cannot be sent is repaired by the retransmit timer.
    fn transmit(socket: &UdpSocket, addr: SocketAddr, datagram: &[u8]) {
        if let Err(err) = socket.send_to(datagram, addr) {
            if err.kind() != io::ErrorKind::WouldBlock {
                log::warn!("data udp send to {addr}: {err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn msg(n: u8) -> Vec<u8> {
        vec![n]
    }

    // --- window state machines ---------------------------------------------

    #[test]
    fn window_fills_continuously_without_waiting_for_acks() {
        let mut send = SlidingWindowSend::new(32, RETRANSMIT_INTERVAL);
        for i in 0..100u8 {
            assert!(send.push(msg(i)));
        }
        let now = t0();
        let first = send.poll_transmit(now);
        // Not stop-and-wait: 32 messages go out back to back with no ACK.
        assert_eq!(first.len(), 32);
        assert_eq!(
            first.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            (0..32).collect::<Vec<_>>()
        );
        assert_eq!(send.inflight_len(), 32);
        assert_eq!(send.pending_len(), 68);
        // Window is full: nothing new until an ACK frees slots.
        assert!(send.poll_transmit(now).is_empty());
        // A cumulative ACK for 10 messages releases exactly 10 new ones.
        send.on_ack(10);
        let next = send.poll_transmit(now);
        assert_eq!(
            next.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            (32..42).collect::<Vec<_>>()
        );
        assert_eq!(send.inflight_len(), 32);
    }

    #[test]
    fn window_size_is_configurable() {
        let mut send = SlidingWindowSend::new(4, RETRANSMIT_INTERVAL);
        for i in 0..10u8 {
            send.push(msg(i));
        }
        assert_eq!(send.poll_transmit(t0()).len(), 4);
    }

    #[test]
    fn unacked_message_is_retransmitted_after_200ms_only() {
        let mut send = SlidingWindowSend::new(8, RETRANSMIT_INTERVAL);
        send.push(msg(1));
        let now = t0();
        assert_eq!(send.poll_transmit(now).len(), 1);
        assert!(
            send.poll_transmit(now + Duration::from_millis(199))
                .is_empty()
        );
        let again = send.poll_transmit(now + Duration::from_millis(200));
        assert_eq!(again, vec![(0, msg(1))]);
        assert_eq!(send.retransmits(), 1);
        // Timer restarted from the retransmission.
        assert!(
            send.poll_transmit(now + Duration::from_millis(399))
                .is_empty()
        );
        assert_eq!(
            send.poll_transmit(now + Duration::from_millis(400)).len(),
            1
        );
        send.on_ack(1);
        assert!(
            send.poll_transmit(now + Duration::from_millis(900))
                .is_empty()
        );
        assert_eq!(send.inflight_len(), 0);
    }

    #[test]
    fn ack_beyond_sent_is_ignored() {
        let mut send = SlidingWindowSend::new(8, RETRANSMIT_INTERVAL);
        send.push(msg(1));
        send.poll_transmit(t0());
        send.on_ack(99);
        assert_eq!(send.inflight_len(), 1);
    }

    #[test]
    fn receiver_delivers_in_order_and_buffers_gaps() {
        let mut recv = SlidingWindowRecv::new(8);
        let out = recv.on_data(1, msg(1));
        assert!(out.delivered.is_empty());
        assert_eq!(out.ack_next, 0);
        let out = recv.on_data(2, msg(2));
        assert!(out.delivered.is_empty());
        let out = recv.on_data(0, msg(0));
        assert_eq!(out.delivered, vec![msg(0), msg(1), msg(2)]);
        assert_eq!(out.ack_next, 3);
        // Duplicate: nothing delivered, still acknowledged.
        let out = recv.on_data(1, msg(1));
        assert!(out.delivered.is_empty());
        assert_eq!(out.ack_next, 3);
        // Beyond the window: refused.
        let out = recv.on_data(3 + 8, msg(9));
        assert!(out.delivered.is_empty());
        assert_eq!(out.ack_next, 3);
    }

    /// Drive sender+receiver over a channel that drops chosen datagrams.
    fn run_lossy(
        total: u8,
        window: u32,
        drop_data: impl Fn(u64, u32) -> bool,
        drop_ack: impl Fn(u64) -> bool,
    ) -> (Vec<Vec<u8>>, u64) {
        let mut send = SlidingWindowSend::new(window, RETRANSMIT_INTERVAL);
        let mut recv = SlidingWindowRecv::new(window);
        for i in 0..total {
            send.push(msg(i));
        }
        let mut delivered = Vec::new();
        let mut attempts: HashMap<u64, u32> = HashMap::new();
        let start = t0();
        let mut ack_count = 0u64;
        // 10 ms steps; plenty to retransmit everything several times.
        for step in 0..2000u32 {
            let now = start + Duration::from_millis(u64::from(step) * 10);
            for (seq, payload) in send.poll_transmit(now) {
                let n = attempts.entry(seq).or_insert(0);
                *n += 1;
                if drop_data(seq, *n) {
                    continue;
                }
                let out = recv.on_data(seq, payload);
                delivered.extend(out.delivered);
                ack_count += 1;
                if !drop_ack(ack_count) {
                    send.on_ack(out.ack_next);
                }
            }
            if delivered.len() == usize::from(total) && send.inflight_len() == 0 {
                break;
            }
        }
        (delivered, send.retransmits())
    }

    #[test]
    fn lost_data_is_retransmitted_and_delivered_in_order() {
        let expected: Vec<Vec<u8>> = (0..50u8).map(msg).collect();
        // First transmission of every 3rd message is lost.
        let (delivered, retransmits) =
            run_lossy(50, 32, |seq, n| seq % 3 == 0 && n == 1, |_| false);
        assert_eq!(delivered, expected);
        assert!(retransmits > 0);
    }

    #[test]
    fn lost_first_message_with_window_full_still_completes() {
        let expected: Vec<Vec<u8>> = (0..40u8).map(msg).collect();
        // Message 0 is lost twice; the rest of the window piles up behind it.
        let (delivered, retransmits) = run_lossy(40, 8, |seq, n| seq == 0 && n <= 2, |_| false);
        assert_eq!(delivered, expected);
        assert!(retransmits >= 2);
    }

    #[test]
    fn lost_acks_do_not_duplicate_or_stall() {
        let expected: Vec<Vec<u8>> = (0..30u8).map(msg).collect();
        // Every other ACK is lost: cumulative ACKs and duplicate re-ACKs repair it.
        let (delivered, _) = run_lossy(30, 8, |_, _| false, |n| n % 2 == 0);
        assert_eq!(delivered, expected);
    }

    // --- framing ----------------------------------------------------------

    #[test]
    fn data_and_ack_round_trip() {
        let data = encode_data("sim-1", "10.0.0.1:8091", 7, b"hello");
        assert_eq!(
            decode(&data),
            Ok(Packet::Data {
                cluster_key: "sim-1",
                sender: "10.0.0.1:8091",
                seq: 7,
                payload: b"hello",
            })
        );
        let ack = encode_ack("sim-1", "10.0.0.2:8091", 42);
        assert_eq!(
            decode(&ack),
            Ok(Packet::Ack {
                cluster_key: "sim-1",
                sender: "10.0.0.2:8091",
                next: 42,
            })
        );
    }

    #[test]
    fn decode_rejects_garbage() {
        assert_eq!(decode(b""), Err(DecodeError::TooShort));
        assert_eq!(decode(b"XX\x01\x01\x00\x00"), Err(DecodeError::BadMagic));
        assert_eq!(
            decode(b"DU\x09\x01\x00\x00"),
            Err(DecodeError::BadVersion(9))
        );
        let mut bad_type = encode_ack("k", "s", 0);
        bad_type[3] = 77;
        assert_eq!(decode(&bad_type), Err(DecodeError::BadType(77)));
        let truncated = encode_data("k", "s", 1, b"x");
        assert_eq!(
            decode(&truncated[..truncated.len() - 3]),
            Err(DecodeError::TooShort)
        );
    }

    #[test]
    fn edge_message_round_trip() {
        let uart = EdgeMessage {
            edge_id: "a:tx->b:rx".into(),
            value: EdgeValue::Uart(UartFrame {
                data: 0x1A5,
                data_bits: 9,
                bit_time: Tick(8680),
                stop_bits: UartStopBits::Two,
                parity: UartParity::Odd,
                inverted: true,
            }),
        };
        assert_eq!(EdgeMessage::decode(&uart.encode()), Some(uart));
        let analog = EdgeMessage {
            edge_id: "a:dac->p:vin".into(),
            value: EdgeValue::Analog(AnalogVoltage(-5_000_000)),
        };
        assert_eq!(EdgeMessage::decode(&analog.encode()), Some(analog));
        assert_eq!(EdgeMessage::decode(&[0, 5, b'a']), None);
        let mut trailing = EdgeMessage {
            edge_id: "e".into(),
            value: EdgeValue::Analog(AnalogVoltage(1)),
        }
        .encode();
        trailing.push(0);
        assert_eq!(EdgeMessage::decode(&trailing), None);
    }

    // --- UDP link ---------------------------------------------------------

    fn nonblocking_socket() -> UdpSocket {
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

    fn pair(window: u32) -> (DataLink, DataLink) {
        let sa = nonblocking_socket();
        let sb = nonblocking_socket();
        let ea = endpoint("a:1", &sa);
        let eb = endpoint("b:1", &sb);
        let a = DataLink::new(sa, "sim", "a:1", &[eb], window).unwrap();
        let b = DataLink::new(sb, "sim", "b:1", &[ea], window).unwrap();
        (a, b)
    }

    fn poll_until<F: FnMut() -> bool>(mut f: F) {
        for _ in 0..400 {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("condition not reached");
    }

    #[test]
    fn link_delivers_in_both_directions_and_acks() {
        let (mut a, mut b) = pair(32);
        for i in 0..100u8 {
            assert!(a.send("b:1", msg(i)).unwrap());
        }
        b.send("a:1", msg(200)).unwrap();
        let mut got_b = Vec::new();
        let mut got_a = Vec::new();
        poll_until(|| {
            got_b.extend(b.poll(Instant::now()).into_iter().map(|(from, m)| {
                assert_eq!(from, "a:1");
                m
            }));
            got_a.extend(a.poll(Instant::now()).into_iter().map(|(from, m)| {
                assert_eq!(from, "b:1");
                m
            }));
            got_b.len() == 100 && got_a.len() == 1 && a.inflight("b:1") == 0
        });
        assert_eq!(got_b, (0..100u8).map(msg).collect::<Vec<_>>());
        assert_eq!(got_a, vec![msg(200)]);
    }

    #[test]
    fn link_retransmits_a_lost_datagram() {
        let sa = nonblocking_socket();
        // Peer socket is read raw first, so the first DATA datagrams are "lost".
        let sb = nonblocking_socket();
        let ea = endpoint("a:1", &sa);
        let eb = endpoint("b:1", &sb);
        let mut a = DataLink::new(sa, "sim", "a:1", &[eb], 4).unwrap();
        a.send("b:1", msg(7)).unwrap();
        let now = Instant::now();
        a.poll(now);
        let mut buf = [0u8; 1500];
        poll_until(|| sb.recv_from(&mut buf).is_ok()); // swallow the first copy
        // Nothing before the retransmit timer ...
        a.poll(now + Duration::from_millis(100));
        assert!(sb.recv_from(&mut buf).is_err());
        // ... and the retransmit after it reaches a now-listening receiver.
        a.poll(now + Duration::from_millis(250));
        let mut b = DataLink::new(sb, "sim", "b:1", &[ea], 4).unwrap();
        let mut got = Vec::new();
        poll_until(|| {
            got.extend(b.poll(Instant::now()));
            !got.is_empty()
        });
        assert_eq!(got, vec![("a:1".to_string(), msg(7))]);
        poll_until(|| {
            a.poll(Instant::now());
            a.inflight("b:1") == 0
        });
    }

    #[test]
    fn link_drops_foreign_cluster_key_and_unknown_sender() {
        let sb = nonblocking_socket();
        let sa = nonblocking_socket();
        let ea = endpoint("a:1", &sa);
        let mut b = DataLink::new(sb.try_clone().unwrap(), "sim", "b:1", &[ea], 8).unwrap();
        let target = sb.local_addr().unwrap();
        // Wrong cluster key, right sender.
        sa.send_to(&encode_data("other", "a:1", 0, b"x"), target)
            .unwrap();
        // Right cluster key, unknown sender.
        sa.send_to(&encode_data("sim", "evil:1", 0, b"x"), target)
            .unwrap();
        // Garbage.
        sa.send_to(b"not a packet", target).unwrap();
        // Valid.
        sa.send_to(&encode_data("sim", "a:1", 0, b"ok"), target)
            .unwrap();
        let mut got = Vec::new();
        poll_until(|| {
            got.extend(b.poll(Instant::now()));
            !got.is_empty()
        });
        assert_eq!(got, vec![("a:1".to_string(), b"ok".to_vec())]);
        assert_eq!(b.stats().dropped_foreign, 3);
    }

    #[test]
    fn link_rejects_bad_configuration() {
        let s = nonblocking_socket();
        let peer = ArbiterEndpoints {
            backend: "b:1".into(),
            advertise_ip: "not-an-ip".into(),
            control_tcp_port: 1,
            data_udp_port: 2,
        };
        assert!(matches!(
            DataLink::new(s.try_clone().unwrap(), "sim", "a:1", &[peer], 8),
            Err(DataLinkError::BadPeerAddress { .. })
        ));
        assert!(matches!(
            DataLink::new(s.try_clone().unwrap(), "sim", "a:1", &[], 0),
            Err(DataLinkError::ZeroWindow)
        ));
        assert!(matches!(
            DataLink::new(s, "", "a:1", &[], 8),
            Err(DataLinkError::BadKeyLength)
        ));
    }
}
