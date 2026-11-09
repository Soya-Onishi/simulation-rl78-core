//! In-process server ↔ arbiter link (two unbounded mpsc channels).
//!
//! Replaces the former iceoryx host buses (`ctrl/s2a`, `status/a2s`, `log/a2s`,
//! action/probe host services). Call sites keep the old port type names; those
//! types are thin adapters over the shared demux below.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use crate::action::{ActionRequest, ActionUpstream};
use crate::host::{HostCommand, SessionStatus};
use crate::log_msg::ClusterLog;

use super::probe_ctrl::{ProbeQueryReplyWire, ProbeQueryWire, ProbeWatchWire};
use super::probe_forward::ProbeToServer;
use super::runtime::IpcError;

/// Server → arbiter messages (one channel, typed variants).
#[derive(Debug)]
pub enum ServerToArbiter {
    Host(HostCommand),
    Action(ActionRequest),
    ProbeWatch(ProbeWatchWire),
    ProbeQuery(ProbeQueryWire),
    ProbeCsv(ProbeQueryWire),
}

/// Arbiter → server messages (one channel, typed variants).
#[derive(Debug)]
pub enum ArbiterToServer {
    Status(SessionStatus),
    /// Boxed: [`ClusterLog`] is ~4KiB and would dominate the enum size.
    Log(Box<ClusterLog>),
    Action(ActionUpstream),
    ProbeCatalog(ProbeToServer),
    ProbeSample(ProbeToServer),
    ProbeQueryReply(ProbeQueryReplyWire),
    ProbeCsvReply(ProbeQueryReplyWire),
}

#[derive(Default)]
struct S2aQueues {
    host: VecDeque<HostCommand>,
    action: VecDeque<ActionRequest>,
    watch: VecDeque<ProbeWatchWire>,
    query: VecDeque<ProbeQueryWire>,
    csv: VecDeque<ProbeQueryWire>,
}

struct S2aDemux {
    rx: Receiver<ServerToArbiter>,
    queues: S2aQueues,
    disconnected: bool,
}

impl S2aDemux {
    fn pump(&mut self) {
        if self.disconnected {
            return;
        }
        loop {
            match self.rx.try_recv() {
                Ok(ServerToArbiter::Host(cmd)) => self.queues.host.push_back(cmd),
                Ok(ServerToArbiter::Action(req)) => self.queues.action.push_back(req),
                Ok(ServerToArbiter::ProbeWatch(w)) => self.queues.watch.push_back(w),
                Ok(ServerToArbiter::ProbeQuery(q)) => self.queues.query.push_back(q),
                Ok(ServerToArbiter::ProbeCsv(q)) => self.queues.csv.push_back(q),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    break;
                }
            }
        }
    }
}

#[derive(Default)]
struct A2sQueues {
    status: VecDeque<SessionStatus>,
    log: VecDeque<ClusterLog>,
    action: VecDeque<ActionUpstream>,
    catalog: VecDeque<ProbeToServer>,
    sample: VecDeque<ProbeToServer>,
    query_reply: VecDeque<ProbeQueryReplyWire>,
    csv_reply: VecDeque<ProbeQueryReplyWire>,
}

struct A2sDemux {
    rx: Receiver<ArbiterToServer>,
    queues: A2sQueues,
}

impl A2sDemux {
    fn pump(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(ArbiterToServer::Status(s)) => self.queues.status.push_back(s),
                Ok(ArbiterToServer::Log(l)) => self.queues.log.push_back(*l),
                Ok(ArbiterToServer::Action(a)) => self.queues.action.push_back(a),
                Ok(ArbiterToServer::ProbeCatalog(p)) => self.queues.catalog.push_back(p),
                Ok(ArbiterToServer::ProbeSample(p)) => self.queues.sample.push_back(p),
                Ok(ArbiterToServer::ProbeQueryReply(r)) => self.queues.query_reply.push_back(r),
                Ok(ArbiterToServer::ProbeCsvReply(r)) => self.queues.csv_reply.push_back(r),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }
}

type S2aRx = Arc<Mutex<S2aDemux>>;
type A2sRx = Arc<Mutex<A2sDemux>>;

fn send_s2a(tx: &Sender<ServerToArbiter>, msg: ServerToArbiter) -> Result<(), IpcError> {
    tx.send(msg)
        .map_err(|_| IpcError::Message("server→arbiter channel closed".into()))
}

fn send_a2s(tx: &Sender<ArbiterToServer>, msg: ArbiterToServer) -> Result<(), IpcError> {
    tx.send(msg)
        .map_err(|_| IpcError::Message("arbiter→server channel closed".into()))
}

fn lock_s2a(rx: &S2aRx) -> Result<std::sync::MutexGuard<'_, S2aDemux>, IpcError> {
    Ok(rx.lock().unwrap_or_else(|err| err.into_inner()))
}

fn lock_a2s(rx: &A2sRx) -> Result<std::sync::MutexGuard<'_, A2sDemux>, IpcError> {
    Ok(rx.lock().unwrap_or_else(|err| err.into_inner()))
}

/// Server-side adapters sharing one receive demux and one send endpoint.
pub struct ServerLink {
    pub host: ServerHostPort,
    pub inbox: ServerLogInbox,
    pub actions: ServerActions,
    pub probe_inbox: ServerProbeInbox,
    pub probe_ctrl: ServerProbeCtrl,
}

/// Arbiter-side adapters sharing one receive demux and one send endpoint.
pub struct ArbiterLink {
    pub host: ArbiterHostPort,
    pub server_actions: ArbiterServerActions,
    pub probe_ctrl_server: ArbiterProbeServer,
    pub probe_forward: ArbiterProbeForward,
    /// Clone into [`super::log_bus::ArbiterLogBus`] for server-bound logs.
    pub log_tx: Sender<ArbiterToServer>,
}

/// Build the two mpsc channels and the port adapters.
pub fn open_pair() -> (ServerLink, ArbiterLink) {
    let (s2a_tx, s2a_rx) = mpsc::channel();
    let (a2s_tx, a2s_rx) = mpsc::channel();
    let s2a_demux = Arc::new(Mutex::new(S2aDemux {
        rx: s2a_rx,
        queues: S2aQueues::default(),
        disconnected: false,
    }));
    let a2s_demux = Arc::new(Mutex::new(A2sDemux {
        rx: a2s_rx,
        queues: A2sQueues::default(),
    }));

    let server = ServerLink {
        host: ServerHostPort {
            tx: s2a_tx.clone(),
            rx: Arc::clone(&a2s_demux),
        },
        inbox: ServerLogInbox {
            rx: Arc::clone(&a2s_demux),
        },
        actions: ServerActions {
            tx: s2a_tx.clone(),
            rx: Arc::clone(&a2s_demux),
        },
        probe_inbox: ServerProbeInbox {
            rx: Arc::clone(&a2s_demux),
        },
        probe_ctrl: ServerProbeCtrl {
            tx: s2a_tx,
            rx: a2s_demux,
        },
    };

    let arbiter = ArbiterLink {
        host: ArbiterHostPort {
            tx: a2s_tx.clone(),
            rx: Arc::clone(&s2a_demux),
        },
        server_actions: ArbiterServerActions {
            tx: a2s_tx.clone(),
            rx: Arc::clone(&s2a_demux),
        },
        probe_ctrl_server: ArbiterProbeServer {
            tx: a2s_tx.clone(),
            rx: Arc::clone(&s2a_demux),
        },
        probe_forward: ArbiterProbeForward {
            tx: a2s_tx.clone(),
        },
        log_tx: a2s_tx,
    };

    (server, arbiter)
}

/// Server-side command publisher and status subscriber.
pub struct ServerHostPort {
    tx: Sender<ServerToArbiter>,
    rx: A2sRx,
}

impl ServerHostPort {
    pub fn publish_cmd(&self, cmd: HostCommand) -> Result<(), IpcError> {
        send_s2a(&self.tx, ServerToArbiter::Host(cmd))
    }

    pub fn try_status(&self) -> Result<Option<SessionStatus>, IpcError> {
        let mut demux = lock_a2s(&self.rx)?;
        demux.pump();
        Ok(demux.queues.status.pop_front())
    }
}

/// Arbiter-side command subscriber and status publisher.
pub struct ArbiterHostPort {
    tx: Sender<ArbiterToServer>,
    rx: S2aRx,
}

impl ArbiterHostPort {
    pub fn try_cmd(&self) -> Result<Option<HostCommand>, IpcError> {
        let mut demux = lock_s2a(&self.rx)?;
        demux.pump();
        if let Some(cmd) = demux.queues.host.pop_front() {
            return Ok(Some(cmd));
        }
        // Server dropped the link (e.g. join timeout abandoned the Simulation):
        // treat as Shutdown so board/plant children are cleaned up.
        if demux.disconnected {
            return Ok(Some(HostCommand::Shutdown));
        }
        Ok(None)
    }

    pub fn publish_status(&self, status: &SessionStatus) -> Result<(), IpcError> {
        send_a2s(&self.tx, ArbiterToServer::Status(status.clone()))
    }
}

/// Server log inbox (arbiter → server).
pub struct ServerLogInbox {
    rx: A2sRx,
}

impl ServerLogInbox {
    pub fn try_recv(&self) -> Result<Option<ClusterLog>, IpcError> {
        let mut demux = lock_a2s(&self.rx)?;
        demux.pump();
        Ok(demux.queues.log.pop_front())
    }

    pub fn drain(&self) -> Result<Vec<ClusterLog>, IpcError> {
        let mut demux = lock_a2s(&self.rx)?;
        demux.pump();
        Ok(demux.queues.log.drain(..).collect())
    }
}

/// Server side: publish action requests, receive upstream.
pub struct ServerActions {
    tx: Sender<ServerToArbiter>,
    rx: A2sRx,
}

impl ServerActions {
    pub fn publish_request(&self, request: &ActionRequest) -> Result<(), IpcError> {
        send_s2a(&self.tx, ServerToArbiter::Action(*request))
    }

    pub fn try_upstream(&self) -> Result<Option<ActionUpstream>, IpcError> {
        let mut demux = lock_a2s(&self.rx)?;
        demux.pump();
        Ok(demux.queues.action.pop_front())
    }
}

/// Arbiter side of the server action channels.
pub struct ArbiterServerActions {
    tx: Sender<ArbiterToServer>,
    rx: S2aRx,
}

impl ArbiterServerActions {
    pub fn try_request(&self) -> Result<Option<ActionRequest>, IpcError> {
        let mut demux = lock_s2a(&self.rx)?;
        demux.pump();
        Ok(demux.queues.action.pop_front())
    }

    pub fn publish_upstream(&self, msg: &ActionUpstream) -> Result<(), IpcError> {
        send_a2s(&self.tx, ArbiterToServer::Action(*msg))
    }
}

/// Server probe catalog/sample inbox.
pub struct ServerProbeInbox {
    rx: A2sRx,
}

impl ServerProbeInbox {
    pub fn try_recv_catalog(&self) -> Result<Option<ProbeToServer>, IpcError> {
        let mut demux = lock_a2s(&self.rx)?;
        demux.pump();
        Ok(demux.queues.catalog.pop_front())
    }

    pub fn try_recv_sample(&self) -> Result<Option<ProbeToServer>, IpcError> {
        let mut demux = lock_a2s(&self.rx)?;
        demux.pump();
        Ok(demux.queues.sample.pop_front())
    }
}

/// Arbiter → server probe forwarder.
pub struct ArbiterProbeForward {
    tx: Sender<ArbiterToServer>,
}

impl ArbiterProbeForward {
    pub fn publish_catalog(
        &self,
        board_hash: crate::ipc::HashedId<u64>,
        probe_id: crate::ipc::HashedId<u64>,
        wire_kind: u8,
        name: &str,
    ) -> Result<(), IpcError> {
        send_a2s(
            &self.tx,
            ArbiterToServer::ProbeCatalog(probe_catalog_sample(
                board_hash, probe_id, wire_kind, name,
            )),
        )
    }

    pub fn publish_catalog_done(&self) -> Result<(), IpcError> {
        send_a2s(
            &self.tx,
            ArbiterToServer::ProbeCatalog(ProbeToServer {
                probe_id: crate::ipc::HashedId::ZERO,
                board_hash: crate::ipc::HashedId::ZERO,
                virtual_time_ns: 0,
                value: 0,
                msg: super::probe_forward::MSG_CATALOG_END,
                wire_kind: 0,
                name_len: 0,
                _pad: [0; 3],
                name: [0; crate::probe::PROBE_NAME_CAP],
            }),
        )
    }

    pub fn publish_sample(
        &self,
        probe_id: crate::ipc::HashedId<u64>,
        virtual_time_ns: u64,
        wire_kind: u8,
        value: u16,
    ) -> Result<(), IpcError> {
        send_a2s(
            &self.tx,
            ArbiterToServer::ProbeSample(ProbeToServer {
                probe_id,
                board_hash: crate::ipc::HashedId::ZERO,
                virtual_time_ns,
                value,
                msg: super::probe_forward::MSG_SAMPLE,
                wire_kind,
                name_len: 0,
                _pad: [0; 3],
                name: [0; crate::probe::PROBE_NAME_CAP],
            }),
        )
    }
}

fn probe_catalog_sample(
    board_hash: crate::ipc::HashedId<u64>,
    probe_id: crate::ipc::HashedId<u64>,
    wire_kind: u8,
    name: &str,
) -> ProbeToServer {
    let bytes = name.as_bytes();
    let len = bytes.len().min(crate::probe::PROBE_NAME_CAP);
    let mut name_buf = [0u8; crate::probe::PROBE_NAME_CAP];
    name_buf[..len].copy_from_slice(&bytes[..len]);
    ProbeToServer {
        probe_id,
        board_hash,
        virtual_time_ns: 0,
        value: 0,
        msg: super::probe_forward::MSG_CATALOG,
        wire_kind,
        name_len: len as u8,
        _pad: [0; 3],
        name: name_buf,
    }
}

/// Server-owned watch/query/csv publishers and reply subscribers.
pub struct ServerProbeCtrl {
    tx: Sender<ServerToArbiter>,
    rx: A2sRx,
}

impl ServerProbeCtrl {
    pub fn publish_watch(&self, sample: ProbeWatchWire) -> Result<(), IpcError> {
        send_s2a(&self.tx, ServerToArbiter::ProbeWatch(sample))
    }

    pub fn publish_query(&self, sample: ProbeQueryWire) -> Result<(), IpcError> {
        send_s2a(&self.tx, ServerToArbiter::ProbeQuery(sample))
    }

    pub fn try_reply(&self) -> Result<Option<ProbeQueryReplyWire>, IpcError> {
        let mut demux = lock_a2s(&self.rx)?;
        demux.pump();
        Ok(demux.queues.query_reply.pop_front())
    }

    pub fn publish_csv(&self, sample: ProbeQueryWire) -> Result<(), IpcError> {
        send_s2a(&self.tx, ServerToArbiter::ProbeCsv(sample))
    }

    pub fn try_csv_reply(&self) -> Result<Option<ProbeQueryReplyWire>, IpcError> {
        let mut demux = lock_a2s(&self.rx)?;
        demux.pump();
        Ok(demux.queues.csv_reply.pop_front())
    }
}

/// Arbiter side of server↔arbiter probe control (node legs stay on iceoryx).
pub struct ArbiterProbeServer {
    tx: Sender<ArbiterToServer>,
    rx: S2aRx,
}

impl ArbiterProbeServer {
    pub fn try_watch(&self) -> Result<Option<ProbeWatchWire>, IpcError> {
        let mut demux = lock_s2a(&self.rx)?;
        demux.pump();
        Ok(demux.queues.watch.pop_front())
    }

    pub fn try_query(&self) -> Result<Option<ProbeQueryWire>, IpcError> {
        let mut demux = lock_s2a(&self.rx)?;
        demux.pump();
        Ok(demux.queues.query.pop_front())
    }

    pub fn try_csv(&self) -> Result<Option<ProbeQueryWire>, IpcError> {
        let mut demux = lock_s2a(&self.rx)?;
        demux.pump();
        Ok(demux.queues.csv.pop_front())
    }

    pub fn publish_reply_to_server(&self, sample: ProbeQueryReplyWire) -> Result<(), IpcError> {
        send_a2s(&self.tx, ArbiterToServer::ProbeQueryReply(sample))
    }

    pub fn publish_csv_to_server(&self, sample: ProbeQueryReplyWire) -> Result<(), IpcError> {
        send_a2s(&self.tx, ArbiterToServer::ProbeCsvReply(sample))
    }
}

/// Send a log record on the arbiter→server channel.
pub fn publish_log(tx: &Sender<ArbiterToServer>, record: ClusterLog) -> Result<(), IpcError> {
    send_a2s(tx, ArbiterToServer::Log(Box::new(record)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_server_link_makes_try_cmd_return_shutdown() {
        let (server, arbiter) = open_pair();
        drop(server);
        assert_eq!(
            arbiter.host.try_cmd().unwrap(),
            Some(HostCommand::Shutdown)
        );
    }
}
