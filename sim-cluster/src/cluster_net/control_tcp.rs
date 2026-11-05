//! Arbiter control plane over TCP (full mesh).
//!
//! Every arbiter of a multi-backend simulation listens on its
//! `PeerListeners::control_tcp` socket and opens one outgoing connection to
//! each peer arbiter (`advertise_ip:control_tcp_port` from the phase-2 request).
//! An outgoing connection is only written to and an accepted connection is only
//! read from, so no connection-direction negotiation is needed.
//!
//! Messages:
//!
//! - [`ControlMessage::Ready`]: all nodes of the sender's machine are Ready.
//!   An arbiter may become startable only when [`ControlMesh::peers_all_ready`]
//!   holds after it has called [`ControlMesh::set_local_ready`].
//! - [`ControlMessage::HostStop`]: a node of the sender's machine reported a
//!   host-initiated stop. The sender writes it to every peer directly and every
//!   receiver publishes `ControlToNode::ClusterStop` to its own nodes. A
//!   receiver never forwards what it received, so a stop cannot circulate in
//!   the mesh; a repeated or older `seq` from the same sender is also dropped.
//!
//! - GDB step coordination, sent only by the arbiter whose machine runs the
//!   stepped board (the *owner*), in this order:
//!   [`ControlMessage::StepBegin`] (every peer stops its nodes and waits),
//!   then, once the stepped board has executed its one instruction,
//!   [`ControlMessage::StepCatchUp`] (every peer runs its nodes until the
//!   stepped board's virtual time `target_vt_ns` and stops there). If the
//!   owner gives the step up before that, it sends
//!   [`ControlMessage::StepCancel`]. `seq` numbers the steps of one sender;
//!   a peer acts on a `StepCatchUp` / `StepCancel` only for the step it was
//!   told about by the matching `StepBegin`. A peer never executes a GdbStep
//!   on a board it does not run.
//!
//! Discipline (same as the time-sync multicast): a frame whose `cluster_key`
//! differs from the local one, that this arbiter sent itself, or whose sender is
//! not a listed peer is discarded.
//!
//! With no peers ([`ControlMesh::open`] returns `None`) no socket is touched:
//! the single-backend path keeps its local Ready handling.
//!
//! New message kinds can be added to [`ControlMessage`] without changing the framing.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::control::HostStopReason;
use crate::orchestrate::ArbiterEndpoints;

const MAGIC: [u8; 4] = *b"RLCT";
const VERSION: u8 = 1;
const KIND_READY: u8 = 1;
const KIND_HOST_STOP: u8 = 2;
const KIND_STEP_BEGIN: u8 = 3;
const KIND_STEP_CATCH_UP: u8 = 4;
const KIND_STEP_CANCEL: u8 = 5;

/// Largest payload (after the 4-byte length prefix) accepted from a peer:
/// magic, version, kind, two strings with their length bytes, and the largest
/// body (`StepCatchUp`: `seq`, `target_vt_ns`, reason).
const MAX_FRAME: usize = 4 + 1 + 1 + (1 + 255) + (1 + 255) + (8 + 8 + 1);

/// Wait between two connect attempts to the same peer.
const RECONNECT_INTERVAL: Duration = Duration::from_millis(100);
/// Limit of one connect attempt (runs on its own thread, never blocks the arbiter loop).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Accepted connections kept open at once; further ones are dropped immediately.
const MAX_INBOUND: usize = 64;

/// Failed to set up the control mesh.
#[derive(Debug, thiserror::Error)]
pub enum ControlMeshError {
    #[error("invalid peer endpoint `{backend}` ({ip}:{port}): expected IPv4 and nonzero port")]
    BadPeer {
        backend: String,
        ip: String,
        port: u16,
    },
    #[error("cluster_key / backend key longer than 255 bytes")]
    KeyTooLong,
    #[error("control TCP socket: {0}")]
    Socket(#[from] std::io::Error),
}

/// Control message exchanged between arbiters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlMessage {
    /// All nodes of the sender's machine are Ready.
    Ready,
    /// A node of the sender's machine reported a host-initiated stop.
    /// `seq` grows by one per stop of one sender and is used to drop repeats.
    HostStop { seq: u64, reason: HostStopReason },
    /// The sender's machine runs the board GDB is stepping: receivers stop
    /// their nodes (if running) and wait for [`Self::StepCatchUp`].
    /// `seq` grows by one per step of one sender.
    StepBegin { seq: u64 },
    /// The stepped board finished its one instruction at `target_vt_ns`:
    /// receivers run their nodes until that virtual time, then stop with
    /// `reason` (what the stepped board reported).
    StepCatchUp {
        seq: u64,
        target_vt_ns: u64,
        reason: HostStopReason,
    },
    /// The sender gave the step `seq` up (host start / stop / reset or a
    /// cluster stop got in between): receivers leave the step and stop their nodes.
    StepCancel { seq: u64 },
}

/// [`ControlMessage`] with the identity of its sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlPacket {
    /// Shared cluster identity (`Phase2ConnectRequest::cluster_key`).
    pub cluster_key: String,
    /// Sending backend, as in `ArbiterEndpoints::backend`.
    pub sender: String,
    pub message: ControlMessage,
}

impl ControlPacket {
    /// Frame layout: `u32` payload length (little endian), then `"RLCT"`,
    /// version, message kind, `len(cluster_key)`, `cluster_key`,
    /// `len(sender)`, `sender`, kind-specific body (all integers are u64 little
    /// endian, a reason is `HostStopReason::to_wire`): `HostStop`: `seq`,
    /// reason; `StepBegin` / `StepCancel`: `seq`; `StepCatchUp`: `seq`,
    /// `target_vt_ns`, reason.
    /// `None` when a string is longer than 255 bytes.
    #[must_use]
    pub fn encode(&self) -> Option<Vec<u8>> {
        let key_len = u8::try_from(self.cluster_key.len()).ok()?;
        let sender_len = u8::try_from(self.sender.len()).ok()?;
        let mut payload = Vec::with_capacity(
            MAGIC.len() + 3 + self.cluster_key.len() + 1 + self.sender.len() + 17,
        );
        payload.extend_from_slice(&MAGIC);
        payload.push(VERSION);
        payload.push(match self.message {
            ControlMessage::Ready => KIND_READY,
            ControlMessage::HostStop { .. } => KIND_HOST_STOP,
            ControlMessage::StepBegin { .. } => KIND_STEP_BEGIN,
            ControlMessage::StepCatchUp { .. } => KIND_STEP_CATCH_UP,
            ControlMessage::StepCancel { .. } => KIND_STEP_CANCEL,
        });
        payload.push(key_len);
        payload.extend_from_slice(self.cluster_key.as_bytes());
        payload.push(sender_len);
        payload.extend_from_slice(self.sender.as_bytes());
        match self.message {
            ControlMessage::Ready => {}
            ControlMessage::HostStop { seq, reason } => {
                payload.extend_from_slice(&seq.to_le_bytes());
                payload.push(reason.to_wire());
            }
            ControlMessage::StepBegin { seq } | ControlMessage::StepCancel { seq } => {
                payload.extend_from_slice(&seq.to_le_bytes());
            }
            ControlMessage::StepCatchUp {
                seq,
                target_vt_ns,
                reason,
            } => {
                payload.extend_from_slice(&seq.to_le_bytes());
                payload.extend_from_slice(&target_vt_ns.to_le_bytes());
                payload.push(reason.to_wire());
            }
        }
        let len = u32::try_from(payload.len()).ok()?;
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&payload);
        Some(frame)
    }

    /// Decode one payload (without the length prefix). `None` for anything
    /// that is not a well-formed version-1 message.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let rest = payload.strip_prefix(&MAGIC)?;
        let (&version, rest) = rest.split_first()?;
        if version != VERSION {
            return None;
        }
        let (&kind, rest) = rest.split_first()?;
        let (cluster_key, rest) = take_string(rest)?;
        let (sender, rest) = take_string(rest)?;
        let message = match kind {
            KIND_READY => {
                if !rest.is_empty() {
                    return None;
                }
                ControlMessage::Ready
            }
            KIND_HOST_STOP => {
                if rest.len() != 9 {
                    return None;
                }
                let seq = u64::from_le_bytes(rest[..8].try_into().ok()?);
                let reason = HostStopReason::from_wire(rest[8])?;
                ControlMessage::HostStop { seq, reason }
            }
            KIND_STEP_BEGIN | KIND_STEP_CANCEL => {
                if rest.len() != 8 {
                    return None;
                }
                let seq = u64::from_le_bytes(rest.try_into().ok()?);
                if kind == KIND_STEP_BEGIN {
                    ControlMessage::StepBegin { seq }
                } else {
                    ControlMessage::StepCancel { seq }
                }
            }
            KIND_STEP_CATCH_UP => {
                if rest.len() != 17 {
                    return None;
                }
                let seq = u64::from_le_bytes(rest[..8].try_into().ok()?);
                let target_vt_ns = u64::from_le_bytes(rest[8..16].try_into().ok()?);
                let reason = HostStopReason::from_wire(rest[16])?;
                ControlMessage::StepCatchUp {
                    seq,
                    target_vt_ns,
                    reason,
                }
            }
            _ => return None,
        };
        Some(Self {
            cluster_key,
            sender,
            message,
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

/// Something the peers told us ([`ControlMesh::poll`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MeshNotice {
    /// `backend` reported all of its nodes Ready (first report only).
    PeerReady { backend: String },
    /// `origin` stopped on a node-reported HostStop. The caller publishes
    /// `ClusterStop` to its own nodes and must **not** send it on to peers.
    ClusterStop {
        origin: String,
        reason: HostStopReason,
    },
    /// `origin` runs the board GDB steps: stop the local nodes and wait for
    /// [`Self::StepCatchUp`]. The local nodes must **not** be asked to step.
    StepBegin { origin: String, seq: u64 },
    /// `origin`'s stepped board is at `target_vt_ns`: run the local nodes until
    /// that virtual time, then stop with `reason`. Only reported for a step
    /// announced by a `StepBegin` that has not been caught up or cancelled yet.
    StepCatchUp {
        origin: String,
        seq: u64,
        target_vt_ns: u64,
        reason: HostStopReason,
    },
    /// `origin` gave the step `seq` up. Only reported for a step that is still
    /// open (not cancelled before).
    StepCancel { origin: String, seq: u64 },
}

/// How far the step a peer announced has come, as seen by this arbiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerStep {
    /// `StepBegin` received, no `StepCatchUp` yet.
    Waiting(u64),
    /// `StepCatchUp` received.
    CaughtUp(u64),
    /// No open step (cancelled or none yet).
    Closed,
}

#[derive(Clone, Copy, Debug)]
struct PeerSlot {
    ready: bool,
    last_stop_seq: u64,
    last_step_seq: u64,
    step: PeerStep,
}

impl Default for PeerSlot {
    fn default() -> Self {
        Self {
            ready: false,
            last_stop_seq: 0,
            last_step_seq: 0,
            step: PeerStep::Closed,
        }
    }
}

/// Socket-free mesh bookkeeping (who is Ready, which stops were seen, which
/// packets are valid). Testable without TCP.
#[derive(Debug)]
pub struct MeshState {
    cluster_key: String,
    self_id: String,
    peers: HashMap<String, PeerSlot>,
    local_ready: bool,
    next_seq: u64,
    next_step_seq: u64,
}

impl MeshState {
    /// `peers` are the other arbiters' backend keys (this arbiter is removed if listed).
    #[must_use]
    pub fn new(
        cluster_key: impl Into<String>,
        self_id: impl Into<String>,
        peers: impl IntoIterator<Item = String>,
    ) -> Self {
        let self_id = self_id.into();
        let peers = peers
            .into_iter()
            .filter(|p| *p != self_id)
            .map(|p| (p, PeerSlot::default()))
            .collect();
        Self {
            cluster_key: cluster_key.into(),
            self_id,
            peers,
            local_ready: false,
            next_seq: 1,
            next_step_seq: 1,
        }
    }

    pub fn set_local_ready(&mut self) {
        self.local_ready = true;
    }

    #[must_use]
    pub fn local_ready(&self) -> bool {
        self.local_ready
    }

    /// Packet announcing this arbiter's Ready.
    #[must_use]
    pub fn ready_packet(&self) -> ControlPacket {
        ControlPacket {
            cluster_key: self.cluster_key.clone(),
            sender: self.self_id.clone(),
            message: ControlMessage::Ready,
        }
    }

    /// Packet announcing a node-reported HostStop; consumes one sequence number.
    pub fn host_stop_packet(&mut self, reason: HostStopReason) -> ControlPacket {
        let seq = self.next_seq;
        self.next_seq += 1;
        ControlPacket {
            cluster_key: self.cluster_key.clone(),
            sender: self.self_id.clone(),
            message: ControlMessage::HostStop { seq, reason },
        }
    }

    fn packet(&self, message: ControlMessage) -> ControlPacket {
        ControlPacket {
            cluster_key: self.cluster_key.clone(),
            sender: self.self_id.clone(),
            message,
        }
    }

    /// Packet opening a GDB step of a local board; consumes one step number
    /// (returned as the first element, to be used in the packets that follow).
    pub fn step_begin_packet(&mut self) -> (u64, ControlPacket) {
        let seq = self.next_step_seq;
        self.next_step_seq += 1;
        (seq, self.packet(ControlMessage::StepBegin { seq }))
    }

    /// Packet telling the peers that the stepped board is at `target_vt_ns`.
    #[must_use]
    pub fn step_catch_up_packet(
        &self,
        seq: u64,
        target_vt_ns: u64,
        reason: HostStopReason,
    ) -> ControlPacket {
        self.packet(ControlMessage::StepCatchUp {
            seq,
            target_vt_ns,
            reason,
        })
    }

    /// Packet telling the peers that step `seq` is given up.
    #[must_use]
    pub fn step_cancel_packet(&self, seq: u64) -> ControlPacket {
        self.packet(ControlMessage::StepCancel { seq })
    }

    /// Every listed peer has reported Ready (vacuously true without peers).
    #[must_use]
    pub fn peers_all_ready(&self) -> bool {
        self.peers.values().all(|p| p.ready)
    }

    /// Peers that have not reported Ready yet, sorted.
    #[must_use]
    pub fn peers_missing(&self) -> Vec<String> {
        let mut missing: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, p)| !p.ready)
            .map(|(id, _)| id.clone())
            .collect();
        missing.sort();
        missing
    }

    /// Fold in a received packet. Returns `None` (and changes nothing) when it
    /// belongs to another cluster, comes from this arbiter, comes from an
    /// unlisted sender, repeats a Ready, or repeats / predates a stop.
    pub fn accept(&mut self, packet: &ControlPacket) -> Option<MeshNotice> {
        if packet.cluster_key != self.cluster_key || packet.sender == self.self_id {
            return None;
        }
        let slot = self.peers.get_mut(&packet.sender)?;
        match packet.message {
            ControlMessage::Ready => {
                if slot.ready {
                    return None;
                }
                slot.ready = true;
                Some(MeshNotice::PeerReady {
                    backend: packet.sender.clone(),
                })
            }
            ControlMessage::HostStop { seq, reason } => {
                if seq <= slot.last_stop_seq {
                    return None;
                }
                slot.last_stop_seq = seq;
                Some(MeshNotice::ClusterStop {
                    origin: packet.sender.clone(),
                    reason,
                })
            }
            ControlMessage::StepBegin { seq } => {
                if seq <= slot.last_step_seq {
                    return None;
                }
                slot.last_step_seq = seq;
                slot.step = PeerStep::Waiting(seq);
                Some(MeshNotice::StepBegin {
                    origin: packet.sender.clone(),
                    seq,
                })
            }
            ControlMessage::StepCatchUp {
                seq,
                target_vt_ns,
                reason,
            } => {
                if slot.step != PeerStep::Waiting(seq) {
                    return None;
                }
                slot.step = PeerStep::CaughtUp(seq);
                Some(MeshNotice::StepCatchUp {
                    origin: packet.sender.clone(),
                    seq,
                    target_vt_ns,
                    reason,
                })
            }
            ControlMessage::StepCancel { seq } => {
                if !matches!(slot.step, PeerStep::Waiting(s) | PeerStep::CaughtUp(s) if s == seq) {
                    return None;
                }
                slot.step = PeerStep::Closed;
                Some(MeshNotice::StepCancel {
                    origin: packet.sender.clone(),
                    seq,
                })
            }
        }
    }
}

enum LinkConn {
    Idle,
    Connecting(Receiver<std::io::Result<TcpStream>>),
    Up(TcpStream),
}

/// Outgoing connection to one peer plus the frames waiting to be written.
struct OutLink {
    addr: SocketAddr,
    conn: LinkConn,
    next_attempt: Instant,
    queue: VecDeque<Vec<u8>>,
    /// Frame being written and how many of its bytes are already out.
    inflight: Option<(Vec<u8>, usize)>,
}

impl OutLink {
    fn new(addr: SocketAddr, now: Instant) -> Self {
        Self {
            addr,
            conn: LinkConn::Idle,
            next_attempt: now,
            queue: VecDeque::new(),
            inflight: None,
        }
    }

    fn is_up(&self) -> bool {
        matches!(self.conn, LinkConn::Up(_))
    }

    /// Connection lost: start the current frame over on the next connection.
    fn drop_connection(&mut self, now: Instant) {
        self.conn = LinkConn::Idle;
        self.next_attempt = now + RECONNECT_INTERVAL;
        if let Some((frame, _)) = self.inflight.take() {
            self.queue.push_front(frame);
        }
    }

    /// Advance connecting and writing. `on_connect` is the frame to send first
    /// on every new connection (Ready, once the local nodes are Ready).
    fn drive(&mut self, now: Instant, on_connect: Option<&[u8]>) {
        if matches!(self.conn, LinkConn::Idle) && now >= self.next_attempt {
            let (tx, rx) = mpsc::channel();
            let addr = self.addr;
            thread::spawn(move || {
                let _ = tx.send(TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT));
            });
            self.conn = LinkConn::Connecting(rx);
        }
        if let LinkConn::Connecting(rx) = &self.conn {
            match rx.try_recv() {
                Ok(Ok(stream)) => {
                    if stream.set_nonblocking(true).is_ok() {
                        let _ = stream.set_nodelay(true);
                        if let Some(frame) = on_connect {
                            self.queue.push_front(frame.to_vec());
                        }
                        self.conn = LinkConn::Up(stream);
                    } else {
                        self.conn = LinkConn::Idle;
                        self.next_attempt = now + RECONNECT_INTERVAL;
                    }
                }
                Ok(Err(err)) => {
                    log::debug!("control TCP connect to {}: {err}", self.addr);
                    self.conn = LinkConn::Idle;
                    self.next_attempt = now + RECONNECT_INTERVAL;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.conn = LinkConn::Idle;
                    self.next_attempt = now + RECONNECT_INTERVAL;
                }
            }
        }
        self.flush(now);
    }

    fn flush(&mut self, now: Instant) {
        loop {
            let LinkConn::Up(stream) = &mut self.conn else {
                return;
            };
            if self.inflight.is_none() {
                self.inflight = self.queue.pop_front().map(|frame| (frame, 0));
            }
            let Some((frame, written)) = self.inflight.as_mut() else {
                return;
            };
            match stream.write(&frame[*written..]) {
                Ok(0) => {
                    self.drop_connection(now);
                    return;
                }
                Ok(n) => {
                    *written += n;
                    if *written == frame.len() {
                        self.inflight = None;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => {
                    log::warn!("control TCP write to {}: {err}", self.addr);
                    if let Some((_, written)) = self.inflight.as_mut() {
                        *written = 0;
                    }
                    self.drop_connection(now);
                    return;
                }
            }
        }
    }
}

/// Accepted connection from a peer arbiter; frames are reassembled in `buf`.
struct Inbound {
    stream: TcpStream,
    buf: Vec<u8>,
}

/// Control TCP full mesh of one arbiter.
pub struct ControlMesh {
    listener: TcpListener,
    state: MeshState,
    links: Vec<OutLink>,
    inbound: Vec<Inbound>,
}

impl ControlMesh {
    /// Join the mesh. `listener` is `PeerListeners::control_tcp` (already
    /// bound, nonblocking); it is duplicated, the caller keeps its own handle.
    /// `peers` are the other arbiters of the simulation.
    ///
    /// Returns `Ok(None)` without touching any socket when there is no peer
    /// (single-backend degenerate path).
    ///
    /// `cluster_key` must be the shared `Phase2ConnectRequest::cluster_key`
    /// (identical in every arbiter), not the member-local iceoryx id.
    pub fn open(
        listener: &TcpListener,
        cluster_key: &str,
        self_id: &str,
        peers: &[ArbiterEndpoints],
        now: Instant,
    ) -> Result<Option<Self>, ControlMeshError> {
        let peers: Vec<&ArbiterEndpoints> = peers.iter().filter(|p| p.backend != self_id).collect();
        if peers.is_empty() {
            return Ok(None);
        }
        if cluster_key.len() > 255 || self_id.len() > 255 {
            return Err(ControlMeshError::KeyTooLong);
        }
        let mut links = Vec::with_capacity(peers.len());
        for peer in &peers {
            let bad = || ControlMeshError::BadPeer {
                backend: peer.backend.clone(),
                ip: peer.advertise_ip.clone(),
                port: peer.control_tcp_port,
            };
            let ip: Ipv4Addr = peer.advertise_ip.parse().map_err(|_| bad())?;
            if peer.control_tcp_port == 0 || peer.backend.len() > 255 {
                return Err(bad());
            }
            links.push(OutLink::new(
                SocketAddr::V4(SocketAddrV4::new(ip, peer.control_tcp_port)),
                now,
            ));
        }
        let state = MeshState::new(
            cluster_key,
            self_id,
            peers.iter().map(|p| p.backend.clone()),
        );
        let listener = listener.try_clone()?;
        listener.set_nonblocking(true)?;
        Ok(Some(Self {
            listener,
            state,
            links,
            inbound: Vec::new(),
        }))
    }

    /// Announce that every node of this arbiter's machine is Ready. Peers learn
    /// it on each outgoing connection, now or when it comes up.
    pub fn set_local_ready(&mut self) {
        if self.state.local_ready() {
            return;
        }
        self.state.set_local_ready();
        if let Some(frame) = self.state.ready_packet().encode() {
            for link in self.links.iter_mut().filter(|l| l.is_up()) {
                link.queue.push_back(frame.clone());
            }
        }
    }

    #[must_use]
    pub fn peers_all_ready(&self) -> bool {
        self.state.peers_all_ready()
    }

    #[must_use]
    pub fn peers_missing(&self) -> Vec<String> {
        self.state.peers_missing()
    }

    /// Tell every peer that a local node reported HostStop, so each of them
    /// publishes ClusterStop to its own nodes. Call it only for a stop that
    /// originated from a local node, never for a [`MeshNotice::ClusterStop`]
    /// received from a peer.
    pub fn broadcast_host_stop(&mut self, reason: HostStopReason) {
        let Some(frame) = self.state.host_stop_packet(reason).encode() else {
            return;
        };
        for link in &mut self.links {
            link.queue.push_back(frame.clone());
        }
    }

    fn broadcast(&mut self, packet: &ControlPacket) {
        let Some(frame) = packet.encode() else {
            return;
        };
        for link in &mut self.links {
            link.queue.push_back(frame.clone());
        }
    }

    /// The local machine runs the board GDB steps: tell every peer to stop its
    /// nodes and wait. Returns the step number to pass to
    /// [`Self::broadcast_step_catch_up`] / [`Self::broadcast_step_cancel`].
    pub fn broadcast_step_begin(&mut self) -> u64 {
        let (seq, packet) = self.state.step_begin_packet();
        self.broadcast(&packet);
        seq
    }

    /// The stepped board executed its instruction and is at `target_vt_ns`:
    /// every peer runs its nodes until then and stops.
    pub fn broadcast_step_catch_up(&mut self, seq: u64, target_vt_ns: u64, reason: HostStopReason) {
        let packet = self.state.step_catch_up_packet(seq, target_vt_ns, reason);
        self.broadcast(&packet);
    }

    /// Give step `seq` up; every peer that still follows it stops its nodes.
    pub fn broadcast_step_cancel(&mut self, seq: u64) {
        let packet = self.state.step_cancel_packet(seq);
        self.broadcast(&packet);
    }

    /// Accept connections, read frames, connect to peers, write queued frames.
    /// Never blocks. Returns what the peers reported since the last call.
    pub fn poll(&mut self, now: Instant) -> Vec<MeshNotice> {
        self.accept_inbound();
        let mut notices = Vec::new();
        self.read_inbound(&mut notices);
        let ready_frame = if self.state.local_ready() {
            self.state.ready_packet().encode()
        } else {
            None
        };
        for link in &mut self.links {
            link.drive(now, ready_frame.as_deref());
        }
        notices
    }

    fn accept_inbound(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.inbound.len() >= MAX_INBOUND || stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    self.inbound.push(Inbound {
                        stream,
                        buf: Vec::new(),
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => {
                    log::warn!("control TCP accept: {err}");
                    return;
                }
            }
        }
    }

    fn read_inbound(&mut self, notices: &mut Vec<MeshNotice>) {
        let state = &mut self.state;
        self.inbound.retain_mut(|conn| {
            let mut chunk = [0u8; 512];
            let mut keep = true;
            loop {
                match conn.stream.read(&mut chunk) {
                    Ok(0) => {
                        keep = false;
                        break;
                    }
                    Ok(n) => conn.buf.extend_from_slice(&chunk[..n]),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        log::debug!("control TCP read: {err}");
                        keep = false;
                        break;
                    }
                }
            }
            // Frames received before the peer closed are still delivered.
            loop {
                if conn.buf.len() < 4 {
                    break;
                }
                let len = u32::from_le_bytes([conn.buf[0], conn.buf[1], conn.buf[2], conn.buf[3]])
                    as usize;
                if len > MAX_FRAME {
                    log::warn!("control TCP: oversized frame ({len} bytes); closing connection");
                    return false;
                }
                if conn.buf.len() < 4 + len {
                    break;
                }
                if let Some(packet) = ControlPacket::decode(&conn.buf[4..4 + len]) {
                    notices.extend(state.accept(&packet));
                }
                conn.buf.drain(..4 + len);
            }
            keep
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sim-1";

    fn ep(backend: &str, port: u16) -> ArbiterEndpoints {
        ArbiterEndpoints {
            backend: backend.into(),
            advertise_ip: "127.0.0.1".into(),
            control_tcp_port: port,
            data_udp_port: 2,
        }
    }

    struct Member {
        id: String,
        listener: TcpListener,
        port: u16,
    }

    fn members(ids: &[&str]) -> Vec<Member> {
        ids.iter()
            .map(|id| {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                listener.set_nonblocking(true).unwrap();
                let port = listener.local_addr().unwrap().port();
                Member {
                    id: (*id).to_string(),
                    listener,
                    port,
                }
            })
            .collect()
    }

    /// One mesh per member; each lists all the others as peers.
    fn full_mesh(members: &[Member], key_of: impl Fn(&Member) -> String) -> Vec<ControlMesh> {
        members
            .iter()
            .map(|me| {
                let peers: Vec<ArbiterEndpoints> = members
                    .iter()
                    .filter(|m| m.id != me.id)
                    .map(|m| ep(&m.id, m.port))
                    .collect();
                ControlMesh::open(&me.listener, &key_of(me), &me.id, &peers, Instant::now())
                    .unwrap()
                    .unwrap()
            })
            .collect()
    }

    /// Poll every mesh until `done` holds or the time is up; collects notices per mesh.
    fn pump(
        meshes: &mut [ControlMesh],
        notices: &mut [Vec<MeshNotice>],
        limit: Duration,
        done: impl Fn(&[ControlMesh], &[Vec<MeshNotice>]) -> bool,
    ) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            for (mesh, got) in meshes.iter_mut().zip(notices.iter_mut()) {
                got.extend(mesh.poll(Instant::now()));
            }
            if done(meshes, notices) {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }

    fn packet(sender: &str, message: ControlMessage) -> ControlPacket {
        ControlPacket {
            cluster_key: KEY.into(),
            sender: sender.into(),
            message,
        }
    }

    #[test]
    fn packet_round_trips_every_message() {
        for message in [
            ControlMessage::Ready,
            ControlMessage::StepBegin { seq: 3 },
            ControlMessage::StepCatchUp {
                seq: 3,
                target_vt_ns: u64::MAX - 1,
                reason: HostStopReason::Step,
            },
            ControlMessage::StepCancel { seq: u64::MAX },
            ControlMessage::HostStop {
                seq: 7,
                reason: HostStopReason::Breakpoint,
            },
        ] {
            let p = packet("10.0.0.1:8091", message);
            let frame = p.encode().unwrap();
            let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
            assert_eq!(len, frame.len() - 4);
            assert_eq!(ControlPacket::decode(&frame[4..]), Some(p));
        }
    }

    #[test]
    fn decode_rejects_garbage_and_bad_reason() {
        assert!(ControlPacket::decode(b"").is_none());
        assert!(ControlPacket::decode(b"XXXX\x01\x01\x00\x00").is_none());
        let mut frame = packet(
            "a",
            ControlMessage::HostStop {
                seq: 1,
                reason: HostStopReason::Until,
            },
        )
        .encode()
        .unwrap();
        let last = frame.len() - 1;
        frame[last] = 99;
        assert!(ControlPacket::decode(&frame[4..]).is_none());
        let long = ControlPacket {
            cluster_key: "k".repeat(256),
            sender: "a".into(),
            message: ControlMessage::Ready,
        };
        assert!(long.encode().is_none());
    }

    #[test]
    fn step_messages_have_a_fixed_body_and_the_largest_frame_fits() {
        let step_frame = |message| packet("a", message).encode().unwrap();
        let begin = step_frame(ControlMessage::StepBegin { seq: 1 });
        let cancel = step_frame(ControlMessage::StepCancel { seq: 1 });
        let catch_up = step_frame(ControlMessage::StepCatchUp {
            seq: 1,
            target_vt_ns: 2,
            reason: HostStopReason::Step,
        });
        // Frame = 4-byte length + 4 magic + version + kind + key + sender + body.
        let head = 4 + 4 + 1 + 1 + (1 + KEY.len()) + (1 + 1);
        assert_eq!(begin.len(), head + 8);
        assert_eq!(cancel.len(), head + 8);
        assert_eq!(catch_up.len(), head + 17);

        // Wrong body lengths and a bad reason are not messages.
        for frame in [&begin, &cancel, &catch_up] {
            let mut longer = frame[4..].to_vec();
            longer.push(0);
            assert!(ControlPacket::decode(&longer).is_none());
            assert!(ControlPacket::decode(&frame[4..frame.len() - 1]).is_none());
        }
        let mut bad_reason = catch_up[4..].to_vec();
        *bad_reason.last_mut().unwrap() = 0;
        assert!(ControlPacket::decode(&bad_reason).is_none());

        // The longest allowed identities still fit `MAX_FRAME`.
        let widest = ControlPacket {
            cluster_key: "k".repeat(255),
            sender: "s".repeat(255),
            message: ControlMessage::StepCatchUp {
                seq: 1,
                target_vt_ns: 2,
                reason: HostStopReason::Step,
            },
        }
        .encode()
        .unwrap();
        assert_eq!(widest.len() - 4, MAX_FRAME);
    }

    #[test]
    fn state_step_follows_begin_then_catch_up_once() {
        let mut s = MeshState::new(KEY, "me", ["a".to_string()]);
        let catch_up = |seq| ControlMessage::StepCatchUp {
            seq,
            target_vt_ns: 500,
            reason: HostStopReason::Step,
        };
        // Catch-up / cancel without a Begin are not news.
        assert_eq!(s.accept(&packet("a", catch_up(1))), None);
        assert_eq!(
            s.accept(&packet("a", ControlMessage::StepCancel { seq: 1 })),
            None
        );

        assert_eq!(
            s.accept(&packet("a", ControlMessage::StepBegin { seq: 1 })),
            Some(MeshNotice::StepBegin {
                origin: "a".into(),
                seq: 1
            })
        );
        // Repeated Begin, catch-up of another step: dropped.
        assert_eq!(
            s.accept(&packet("a", ControlMessage::StepBegin { seq: 1 })),
            None
        );
        assert_eq!(s.accept(&packet("a", catch_up(2))), None);
        assert_eq!(
            s.accept(&packet("a", catch_up(1))),
            Some(MeshNotice::StepCatchUp {
                origin: "a".into(),
                seq: 1,
                target_vt_ns: 500,
                reason: HostStopReason::Step,
            })
        );
        assert_eq!(s.accept(&packet("a", catch_up(1))), None);
        // Strangers and ourselves cannot steer a step.
        assert_eq!(
            s.accept(&packet("stranger", ControlMessage::StepBegin { seq: 9 })),
            None
        );
        assert_eq!(
            s.accept(&packet("me", ControlMessage::StepBegin { seq: 9 })),
            None
        );
    }

    #[test]
    fn state_step_cancel_closes_the_open_step_only() {
        let mut s = MeshState::new(KEY, "me", ["a".to_string()]);
        let cancel = |seq| packet("a", ControlMessage::StepCancel { seq });
        s.accept(&packet("a", ControlMessage::StepBegin { seq: 4 }));
        // Another step number: not ours to cancel.
        assert_eq!(s.accept(&cancel(3)), None);
        assert_eq!(
            s.accept(&cancel(4)),
            Some(MeshNotice::StepCancel {
                origin: "a".into(),
                seq: 4
            })
        );
        assert_eq!(s.accept(&cancel(4)), None);
        // After the cancel a catch-up of that step is dropped and a stale Begin
        // cannot reopen it; a newer Begin can.
        assert_eq!(
            s.accept(&packet(
                "a",
                ControlMessage::StepCatchUp {
                    seq: 4,
                    target_vt_ns: 1,
                    reason: HostStopReason::Step
                }
            )),
            None
        );
        assert_eq!(
            s.accept(&packet("a", ControlMessage::StepBegin { seq: 4 })),
            None
        );
        assert!(
            s.accept(&packet("a", ControlMessage::StepBegin { seq: 5 }))
                .is_some()
        );
    }

    #[test]
    fn step_packets_use_increasing_step_numbers_and_carry_the_sender() {
        let mut s = MeshState::new(KEY, "me", ["a".to_string()]);
        let (first, begin) = s.step_begin_packet();
        let (second, _) = s.step_begin_packet();
        assert_eq!((first, second), (1, 2));
        assert_eq!(begin.sender, "me");
        assert_eq!(begin.message, ControlMessage::StepBegin { seq: 1 });
        assert_eq!(
            s.step_catch_up_packet(2, 70, HostStopReason::Breakpoint)
                .message,
            ControlMessage::StepCatchUp {
                seq: 2,
                target_vt_ns: 70,
                reason: HostStopReason::Breakpoint
            }
        );
        assert_eq!(
            s.step_cancel_packet(2).message,
            ControlMessage::StepCancel { seq: 2 }
        );
    }

    #[test]
    fn state_ready_barrier_needs_every_peer() {
        let mut s = MeshState::new(KEY, "me", ["a".to_string(), "b".to_string()]);
        assert!(!s.peers_all_ready());
        assert_eq!(s.peers_missing(), ["a", "b"]);
        assert_eq!(
            s.accept(&packet("a", ControlMessage::Ready)),
            Some(MeshNotice::PeerReady {
                backend: "a".into()
            })
        );
        assert!(!s.peers_all_ready());
        // A repeated Ready is not news.
        assert_eq!(s.accept(&packet("a", ControlMessage::Ready)), None);
        s.accept(&packet("b", ControlMessage::Ready));
        assert!(s.peers_all_ready());
        assert!(s.peers_missing().is_empty());
    }

    #[test]
    fn state_without_peers_is_always_ready() {
        let s = MeshState::new(KEY, "me", Vec::<String>::new());
        assert!(s.peers_all_ready());
        // Listing ourselves does not create a peer either.
        let s = MeshState::new(KEY, "me", ["me".to_string()]);
        assert!(s.peers_all_ready());
    }

    #[test]
    fn state_discards_foreign_cluster_self_and_unlisted_sender() {
        let mut s = MeshState::new(KEY, "me", ["a".to_string()]);
        let mut other = packet("a", ControlMessage::Ready);
        other.cluster_key = "other".into();
        assert_eq!(s.accept(&other), None);
        assert_eq!(s.accept(&packet("me", ControlMessage::Ready)), None);
        assert_eq!(s.accept(&packet("stranger", ControlMessage::Ready)), None);
        assert!(!s.peers_all_ready());
        let stop = ControlMessage::HostStop {
            seq: 1,
            reason: HostStopReason::Breakpoint,
        };
        assert_eq!(s.accept(&packet("stranger", stop)), None);
    }

    #[test]
    fn state_drops_repeated_and_older_stops() {
        let mut s = MeshState::new(KEY, "me", ["a".to_string()]);
        let stop = |seq| ControlMessage::HostStop {
            seq,
            reason: HostStopReason::Breakpoint,
        };
        let notice = Some(MeshNotice::ClusterStop {
            origin: "a".into(),
            reason: HostStopReason::Breakpoint,
        });
        assert_eq!(s.accept(&packet("a", stop(2))), notice);
        assert_eq!(s.accept(&packet("a", stop(2))), None);
        assert_eq!(s.accept(&packet("a", stop(1))), None);
        assert_eq!(s.accept(&packet("a", stop(3))), notice);
    }

    #[test]
    fn host_stop_packets_use_increasing_seq() {
        let mut s = MeshState::new(KEY, "me", ["a".to_string()]);
        let seq = |p: ControlPacket| match p.message {
            ControlMessage::HostStop { seq, .. } => seq,
            other => panic!("expected HostStop, got {other:?}"),
        };
        assert_eq!(seq(s.host_stop_packet(HostStopReason::Step)), 1);
        assert_eq!(seq(s.host_stop_packet(HostStopReason::Step)), 2);
    }

    #[test]
    fn open_without_peers_is_none_and_touches_no_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(
            ControlMesh::open(&listener, KEY, "me", &[], Instant::now())
                .unwrap()
                .is_none()
        );
        // Only ourselves listed: still no peer.
        let port = listener.local_addr().unwrap().port();
        assert!(
            ControlMesh::open(&listener, KEY, "me", &[ep("me", port)], Instant::now())
                .unwrap()
                .is_none()
        );
        // Nobody connected to the listener.
        assert!(listener.accept().is_err());
    }

    #[test]
    fn open_rejects_bad_peer_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut bad_ip = ep("a", 5);
        bad_ip.advertise_ip = "not-an-ip".into();
        assert!(ControlMesh::open(&listener, KEY, "me", &[bad_ip], Instant::now()).is_err());
        assert!(ControlMesh::open(&listener, KEY, "me", &[ep("a", 0)], Instant::now()).is_err());
    }

    #[test]
    fn mesh_ready_barrier_waits_for_every_member() {
        let ms = members(&["a:1", "b:1", "c:1"]);
        let mut meshes = full_mesh(&ms, |_| KEY.into());
        let mut notices = vec![Vec::new(); 3];

        // Only `a` and `b` are Ready: nobody may pass the barrier.
        meshes[0].set_local_ready();
        meshes[1].set_local_ready();
        pump(
            &mut meshes,
            &mut notices,
            Duration::from_millis(300),
            |_, _| false,
        );
        assert!(!meshes[0].peers_all_ready());
        assert_eq!(meshes[0].peers_missing(), ["c:1"]);
        assert!(!meshes[1].peers_all_ready());
        // `c` has heard both Ready messages, but it is not Ready itself.
        assert!(meshes[2].peers_all_ready());
        assert!(!meshes[2].state.local_ready());

        // `c` follows (its Ready is written to connections that are already up).
        meshes[2].set_local_ready();
        let done = pump(
            &mut meshes,
            &mut notices,
            Duration::from_secs(3),
            |meshes, _| meshes.iter().all(ControlMesh::peers_all_ready),
        );
        assert!(done, "all members must see all peers Ready");
        assert!(notices[0].contains(&MeshNotice::PeerReady {
            backend: "c:1".into()
        }));
    }

    #[test]
    fn ready_sent_before_connection_is_up_is_not_lost() {
        let ms = members(&["a:1", "b:1"]);
        let mut meshes = full_mesh(&ms, |_| KEY.into());
        let mut notices = vec![Vec::new(); 2];
        // Ready is set before the first poll, hence before any connection exists.
        meshes[0].set_local_ready();
        meshes[1].set_local_ready();
        let done = pump(
            &mut meshes,
            &mut notices,
            Duration::from_secs(3),
            |meshes, _| meshes.iter().all(ControlMesh::peers_all_ready),
        );
        assert!(done);
    }

    #[test]
    fn host_stop_reaches_every_peer_once_and_is_not_echoed() {
        let ms = members(&["a:1", "b:1", "c:1"]);
        let mut meshes = full_mesh(&ms, |_| KEY.into());
        let mut notices = vec![Vec::new(); 3];
        for m in &mut meshes {
            m.set_local_ready();
        }
        assert!(pump(
            &mut meshes,
            &mut notices,
            Duration::from_secs(3),
            |meshes, _| meshes.iter().all(ControlMesh::peers_all_ready),
        ));

        meshes[0].broadcast_host_stop(HostStopReason::Breakpoint);
        let stop = MeshNotice::ClusterStop {
            origin: "a:1".into(),
            reason: HostStopReason::Breakpoint,
        };
        assert!(pump(
            &mut meshes,
            &mut notices,
            Duration::from_secs(3),
            |_, notices| notices[1].contains(&stop) && notices[2].contains(&stop),
        ));
        // Let any stray duplicate arrive before counting.
        pump(
            &mut meshes,
            &mut notices,
            Duration::from_millis(200),
            |_, _| false,
        );
        let count = |i: usize| notices[i].iter().filter(|n| **n == stop).count();
        assert_eq!(count(1), 1);
        assert_eq!(count(2), 1);
        // The origin hears nothing back: receivers never forward a stop.
        assert_eq!(count(0), 0);
    }

    #[test]
    fn step_messages_reach_every_peer_in_order_and_are_not_echoed() {
        let ms = members(&["a:1", "b:1", "c:1"]);
        let mut meshes = full_mesh(&ms, |_| KEY.into());
        let mut notices = vec![Vec::new(); 3];
        for m in &mut meshes {
            m.set_local_ready();
        }
        assert!(pump(
            &mut meshes,
            &mut notices,
            Duration::from_secs(3),
            |meshes, _| meshes.iter().all(ControlMesh::peers_all_ready),
        ));

        let seq = meshes[0].broadcast_step_begin();
        meshes[0].broadcast_step_catch_up(seq, 1234, HostStopReason::Step);
        let steps = |got: &[MeshNotice]| -> Vec<MeshNotice> {
            got.iter()
                .filter(|n| {
                    matches!(
                        n,
                        MeshNotice::StepBegin { .. }
                            | MeshNotice::StepCatchUp { .. }
                            | MeshNotice::StepCancel { .. }
                    )
                })
                .cloned()
                .collect()
        };
        let expected = vec![
            MeshNotice::StepBegin {
                origin: "a:1".into(),
                seq,
            },
            MeshNotice::StepCatchUp {
                origin: "a:1".into(),
                seq,
                target_vt_ns: 1234,
                reason: HostStopReason::Step,
            },
        ];
        assert!(pump(
            &mut meshes,
            &mut notices,
            Duration::from_secs(3),
            |_, notices| steps(&notices[1]).len() == 2 && steps(&notices[2]).len() == 2,
        ));
        pump(
            &mut meshes,
            &mut notices,
            Duration::from_millis(200),
            |_, _| false,
        );
        assert_eq!(steps(&notices[1]), expected);
        assert_eq!(steps(&notices[2]), expected);
        assert!(
            steps(&notices[0]).is_empty(),
            "the owner hears nothing back"
        );

        // A cancel of a later step reaches the peers that follow it.
        let seq2 = meshes[0].broadcast_step_begin();
        meshes[0].broadcast_step_cancel(seq2);
        let cancel = MeshNotice::StepCancel {
            origin: "a:1".into(),
            seq: seq2,
        };
        assert!(pump(
            &mut meshes,
            &mut notices,
            Duration::from_secs(3),
            |_, notices| notices[1].contains(&cancel) && notices[2].contains(&cancel),
        ));
    }

    #[test]
    fn foreign_cluster_key_is_discarded_over_tcp() {
        let ms = members(&["a:1", "b:1"]);
        let mut meshes = full_mesh(&ms, |m| {
            if m.id == "a:1" {
                KEY.into()
            } else {
                "other-sim".into()
            }
        });
        let mut notices = vec![Vec::new(); 2];
        meshes[0].set_local_ready();
        meshes[1].set_local_ready();
        meshes[1].broadcast_host_stop(HostStopReason::Breakpoint);
        pump(
            &mut meshes,
            &mut notices,
            Duration::from_millis(500),
            |_, _| false,
        );
        assert!(!meshes[0].peers_all_ready());
        assert!(!meshes[1].peers_all_ready());
        assert!(notices[0].is_empty());
        assert!(notices[1].is_empty());
    }

    #[test]
    fn oversized_frame_closes_the_connection_without_breaking_the_mesh() {
        let ms = members(&["a:1", "b:1"]);
        let mut meshes = full_mesh(&ms, |_| KEY.into());
        let mut notices = vec![Vec::new(); 2];

        let mut rogue = TcpStream::connect(("127.0.0.1", ms[0].port)).unwrap();
        rogue.write_all(&u32::MAX.to_le_bytes()).unwrap();
        rogue.write_all(b"junk").unwrap();
        // Garbage of the right length is skipped as well.
        let mut rogue2 = TcpStream::connect(("127.0.0.1", ms[0].port)).unwrap();
        rogue2.write_all(&3u32.to_le_bytes()).unwrap();
        rogue2.write_all(b"abc").unwrap();

        meshes[0].set_local_ready();
        meshes[1].set_local_ready();
        assert!(pump(
            &mut meshes,
            &mut notices,
            Duration::from_secs(3),
            |meshes, _| meshes.iter().all(ControlMesh::peers_all_ready),
        ));
        assert_eq!(
            meshes[0].inbound.len(),
            2,
            "rogue1 closed, rogue2 + b:1 remain"
        );
    }
}
