//! Arbiter → server probe channel.
//!
//! One ordered stream carries catalog rows and samples. The server creates the
//! service before the arbiter opens it. Safe overflow drops samples that do
//! not fit; those samples are absent from the server log.

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::probe::PROBE_NAME_CAP;

use super::names;
use super::runtime::IpcError;

pub const MSG_CATALOG: u8 = 1;
pub const MSG_SAMPLE: u8 = 2;

/// Subscriber slots. Bursts beyond this are dropped by the transport.
pub const PROBE_A2S_BUFFER: usize = 8192;

#[repr(C)]
#[derive(Clone, Copy, Debug, ZeroCopySend)]
pub struct ProbeToServer {
    pub probe_id: u64,
    pub board_hash: u64,
    pub virtual_time_ns: u64,
    pub value: u16,
    pub msg: u8,
    pub wire_kind: u8,
    pub name_len: u8,
    pub _pad: [u8; 3],
    pub name: [u8; PROBE_NAME_CAP],
}

type ProbePub = Publisher<ipc::Service, ProbeToServer, ()>;
type ProbeSub = Subscriber<ipc::Service, ProbeToServer, ()>;

fn svc_name(name: &str) -> Result<ServiceName, IpcError> {
    name.try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))
}

fn factory(
    node: &Node<ipc::Service>,
    name: &str,
    create: bool,
) -> Result<
    iceoryx2::service::port_factory::publish_subscribe::PortFactory<
        ipc::Service,
        ProbeToServer,
        (),
    >,
    IpcError,
> {
    let builder = node
        .service_builder(&svc_name(name)?)
        .publish_subscribe::<ProbeToServer>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(4)
        .subscriber_max_buffer_size(PROBE_A2S_BUFFER)
        .history_size(0)
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

pub struct ServerProbeInbox {
    sub: ProbeSub,
}

impl ServerProbeInbox {
    pub fn create(node: &Node<ipc::Service>, cluster_key: &str) -> Result<Self, IpcError> {
        let name = names::probe_a2s(cluster_key);
        let factory = factory(node, &name, true)?;
        let sub = factory
            .subscriber_builder()
            .create()
            .map_err(|e| IpcError::Message(format!("probe subscriber {name}: {e:?}")))?;
        Ok(Self { sub })
    }

    pub fn try_recv(&self) -> Result<Option<ProbeToServer>, IpcError> {
        match self
            .sub
            .receive()
            .map_err(|e| IpcError::Message(format!("probe a2s receive: {e:?}")))?
        {
            Some(sample) => Ok(Some(*sample)),
            None => Ok(None),
        }
    }
}

pub struct ArbiterProbeForward {
    pub_port: ProbePub,
}

impl ArbiterProbeForward {
    pub fn open(node: &Node<ipc::Service>, cluster_key: &str) -> Result<Self, IpcError> {
        let name = names::probe_a2s(cluster_key);
        let factory = factory(node, &name, false)?;
        let pub_port = factory
            .publisher_builder()
            .create()
            .map_err(|e| IpcError::Message(format!("probe publisher {name}: {e:?}")))?;
        Ok(Self { pub_port })
    }

    pub fn publish_catalog(
        &self,
        board_hash: u64,
        probe_id: u64,
        wire_kind: u8,
        name: &str,
    ) -> Result<(), IpcError> {
        let mut sample = ProbeToServer {
            probe_id,
            board_hash,
            virtual_time_ns: 0,
            value: 0,
            msg: MSG_CATALOG,
            wire_kind,
            name_len: 0,
            _pad: [0; 3],
            name: [0; PROBE_NAME_CAP],
        };
        let n = name.len().min(PROBE_NAME_CAP);
        sample.name[..n].copy_from_slice(&name.as_bytes()[..n]);
        sample.name_len = u8::try_from(n).unwrap_or(u8::MAX);
        self.send(sample)
    }

    pub fn publish_sample(
        &self,
        probe_id: u64,
        virtual_time_ns: u64,
        wire_kind: u8,
        value: u16,
    ) -> Result<(), IpcError> {
        self.send(ProbeToServer {
            probe_id,
            board_hash: 0,
            virtual_time_ns,
            value,
            msg: MSG_SAMPLE,
            wire_kind,
            name_len: 0,
            _pad: [0; 3],
            name: [0; PROBE_NAME_CAP],
        })
    }

    fn send(&self, sample: ProbeToServer) -> Result<(), IpcError> {
        self.pub_port
            .send_copy(sample)
            .map_err(|e| IpcError::Message(format!("probe a2s send: {e:?}")))?;
        Ok(())
    }
}
