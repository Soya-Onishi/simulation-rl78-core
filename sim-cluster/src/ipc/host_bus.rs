//! Server ↔ arbiter host commands (`ctrl/s2a`) and session status (`status/a2s`).
//!
//! The server creates both services before spawning the arbiter. Commands are
//! one publisher (server) and one subscriber (arbiter). Status is the reverse.
//! History on the command service keeps a `start` that arrives before the
//! arbiter finishes opening.

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::host::{HostCommand, SessionNode, SessionStatus};

use super::names;
use super::runtime::IpcError;

const ID_CAP: usize = 64;
const KIND_CAP: usize = 32;
const NODE_CAP: usize = 32;

/// Server → arbiter command sample.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum HostCommandWire {
    Start = 1,
    Stop = 2,
    Reset = 3,
    Shutdown = 4,
}

impl HostCommandWire {
    fn from_cmd(cmd: HostCommand) -> Self {
        match cmd {
            HostCommand::Start => Self::Start,
            HostCommand::Stop => Self::Stop,
            HostCommand::Reset => Self::Reset,
            HostCommand::Shutdown => Self::Shutdown,
        }
    }

    fn to_cmd(self) -> HostCommand {
        match self {
            Self::Start => HostCommand::Start,
            Self::Stop => HostCommand::Stop,
            Self::Reset => HostCommand::Reset,
            Self::Shutdown => HostCommand::Shutdown,
        }
    }
}

/// Peer label carried in a status sample.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub enum HostPeerWire {
    Unknown = 0,
    AwaitingReady = 1,
    Ready = 2,
    Running = 3,
    Stopped = 4,
}

impl HostPeerWire {
    fn from_label(label: &str) -> Self {
        match label {
            "awaiting_ready" => Self::AwaitingReady,
            "ready" => Self::Ready,
            "running" => Self::Running,
            "stopped" => Self::Stopped,
            _ => Self::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::AwaitingReady => "awaiting_ready",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
struct HostNodeWire {
    id: [u8; ID_CAP],
    id_len: u16,
    kind: [u8; KIND_CAP],
    kind_len: u16,
    peer: HostPeerWire,
}

impl Default for HostNodeWire {
    fn default() -> Self {
        Self {
            id: [0; ID_CAP],
            id_len: 0,
            kind: [0; KIND_CAP],
            kind_len: 0,
            peer: HostPeerWire::Unknown,
        }
    }
}

/// One arbiter → server status sample.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub struct HostStatusWire {
    running: u8,
    virtual_time_ns: u64,
    node_count: u16,
    nodes: [HostNodeWire; NODE_CAP],
}

impl HostStatusWire {
    fn from_status(status: &SessionStatus) -> Self {
        let mut nodes = [HostNodeWire::default(); NODE_CAP];
        let count = status.nodes.len().min(NODE_CAP);
        for (slot, node) in nodes.iter_mut().zip(status.nodes.iter()).take(count) {
            let mut id = [0u8; ID_CAP];
            let mut kind = [0u8; KIND_CAP];
            *slot = HostNodeWire {
                id_len: pack_into(&node.id, &mut id),
                id,
                kind_len: pack_into(&node.kind, &mut kind),
                kind,
                peer: HostPeerWire::from_label(&node.state),
            };
        }
        Self {
            running: u8::from(status.state == "running"),
            virtual_time_ns: status.virtual_time_ns,
            node_count: u16::try_from(count).unwrap_or(u16::MAX),
            nodes,
        }
    }

    fn to_status(self) -> SessionStatus {
        let count = usize::from(self.node_count).min(NODE_CAP);
        let nodes = self.nodes[..count]
            .iter()
            .map(|node| SessionNode {
                id: unpack(&node.id, node.id_len),
                kind: unpack(&node.kind, node.kind_len),
                state: node.peer.label().to_string(),
            })
            .collect();
        SessionStatus {
            state: if self.running == 1 {
                "running".to_string()
            } else {
                "stopped".to_string()
            },
            virtual_time_ns: self.virtual_time_ns,
            nodes,
        }
    }
}

type CmdPub = Publisher<ipc::Service, HostCommandWire, ()>;
type CmdSub = Subscriber<ipc::Service, HostCommandWire, ()>;
type StatusPub = Publisher<ipc::Service, HostStatusWire, ()>;
type StatusSub = Subscriber<ipc::Service, HostStatusWire, ()>;

/// Server-side command publisher and status subscriber.
pub struct ServerHostPort {
    cmd: CmdPub,
    status: StatusSub,
}

impl ServerHostPort {
    /// Create both services. Call before the arbiter process starts.
    pub fn create(node: &Node<ipc::Service>, cluster_key: &str) -> Result<Self, IpcError> {
        let cmd_name = names::ctrl_s2a(cluster_key);
        let status_name = names::status_a2s(cluster_key);
        let cmd = publisher::<HostCommandWire>(node, &cmd_name, true)?;
        let status = subscriber::<HostStatusWire>(node, &status_name, true)?;
        Ok(Self { cmd, status })
    }

    pub fn publish_cmd(&self, cmd: HostCommand) -> Result<(), IpcError> {
        self.cmd
            .send_copy(HostCommandWire::from_cmd(cmd))
            .map_err(|e| IpcError::Message(format!("s2a send: {e:?}")))?;
        Ok(())
    }

    pub fn try_status(&self) -> Result<Option<SessionStatus>, IpcError> {
        match self
            .status
            .receive()
            .map_err(|e| IpcError::Message(format!("status a2s receive: {e:?}")))?
        {
            Some(sample) => Ok(Some((*sample).to_status())),
            None => Ok(None),
        }
    }
}

/// Arbiter-side command subscriber and status publisher.
pub struct ArbiterHostPort {
    cmd: CmdSub,
    status: StatusPub,
}

impl ArbiterHostPort {
    pub fn open(node: &Node<ipc::Service>, cluster_key: &str) -> Result<Self, IpcError> {
        let cmd_name = names::ctrl_s2a(cluster_key);
        let status_name = names::status_a2s(cluster_key);
        let cmd = subscriber::<HostCommandWire>(node, &cmd_name, false)?;
        let status = publisher::<HostStatusWire>(node, &status_name, false)?;
        Ok(Self { cmd, status })
    }

    pub fn try_cmd(&self) -> Result<Option<HostCommand>, IpcError> {
        match self
            .cmd
            .receive()
            .map_err(|e| IpcError::Message(format!("s2a receive: {e:?}")))?
        {
            Some(sample) => Ok(Some(sample.to_cmd())),
            None => Ok(None),
        }
    }

    pub fn publish_status(&self, status: &SessionStatus) -> Result<(), IpcError> {
        self.status
            .send_copy(HostStatusWire::from_status(status))
            .map_err(|e| IpcError::Message(format!("status a2s send: {e:?}")))?;
        Ok(())
    }
}

fn publisher<T: ZeroCopySend + std::fmt::Debug>(
    node: &Node<ipc::Service>,
    name: &str,
    create: bool,
) -> Result<Publisher<ipc::Service, T, ()>, IpcError> {
    let svc = service::<T>(node, name, create)?;
    svc.publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("publisher {name}: {e:?}")))
}

fn subscriber<T: ZeroCopySend + std::fmt::Debug>(
    node: &Node<ipc::Service>,
    name: &str,
    create: bool,
) -> Result<Subscriber<ipc::Service, T, ()>, IpcError> {
    let svc = service::<T>(node, name, create)?;
    svc.subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("subscriber {name}: {e:?}")))
}

fn service<T: ZeroCopySend + std::fmt::Debug>(
    node: &Node<ipc::Service>,
    name: &str,
    create: bool,
) -> Result<
    iceoryx2::service::port_factory::publish_subscribe::PortFactory<ipc::Service, T, ()>,
    IpcError,
> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let builder = node
        .service_builder(&svc_name)
        .publish_subscribe::<T>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(4)
        .subscriber_max_buffer_size(16)
        .history_size(16)
        .enable_safe_overflow(true);
    if create {
        builder
            .create()
            .map_err(|e| IpcError::Message(format!("create {name}: {e:?}")))
    } else {
        builder
            .open_or_create()
            .map_err(|e| IpcError::Message(format!("open {name}: {e:?}")))
    }
}

fn fit_len(text: &str, cap: usize) -> usize {
    let mut end = text.len().min(cap);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn pack_into(text: &str, buf: &mut [u8]) -> u16 {
    let n = fit_len(text, buf.len());
    buf[..n].copy_from_slice(&text.as_bytes()[..n]);
    u16::try_from(n).unwrap_or(u16::MAX)
}

fn unpack(buf: &[u8], len: u16) -> String {
    let n = usize::from(len).min(buf.len());
    match std::str::from_utf8(&buf[..n]) {
        Ok(text) => text.to_string(),
        Err(err) => String::from_utf8_lossy(&buf[..err.valid_up_to()]).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trip_keeps_nodes() {
        let status = SessionStatus {
            state: "running".into(),
            virtual_time_ns: 42,
            nodes: vec![SessionNode {
                id: "board-a".into(),
                kind: "rl78".into(),
                state: "running".into(),
            }],
        };
        let back = HostStatusWire::from_status(&status).to_status();
        assert_eq!(back, status);
    }

    #[test]
    fn command_round_trip() {
        for cmd in [
            HostCommand::Start,
            HostCommand::Stop,
            HostCommand::Reset,
            HostCommand::Shutdown,
        ] {
            assert_eq!(HostCommandWire::from_cmd(cmd).to_cmd(), cmd);
        }
    }
}
