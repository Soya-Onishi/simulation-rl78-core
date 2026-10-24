//! Arbiter → server probe channels.
//!
//! Catalog rows and live samples use separate services so a sample burst cannot
//! overflow-drop the one-shot catalog dump. The server creates both services
//! before the arbiter opens them.

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::probe::PROBE_NAME_CAP;

use super::names;
use super::runtime::IpcError;

pub const MSG_CATALOG: u8 = 1;
pub const MSG_SAMPLE: u8 = 2;
pub const MSG_CATALOG_END: u8 = 3;

/// Subscriber slots. Bursts beyond this are dropped by the transport.
pub const PROBE_A2S_BUFFER: usize = 8192;
pub const PROBE_CATALOG_A2S_BUFFER: usize = 256;

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
    buffer: usize,
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
        .subscriber_max_buffer_size(buffer)
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
    catalog: ProbeSub,
    samples: ProbeSub,
}

fn open_sub(
    node: &Node<ipc::Service>,
    name: &str,
    create: bool,
    buffer: usize,
) -> Result<ProbeSub, IpcError> {
    factory(node, name, create, buffer)?
        .subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("probe subscriber {name}: {e:?}")))
}

fn open_pub(
    node: &Node<ipc::Service>,
    name: &str,
    create: bool,
    buffer: usize,
) -> Result<ProbePub, IpcError> {
    factory(node, name, create, buffer)?
        .publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("probe publisher {name}: {e:?}")))
}

fn recv_one(sub: &ProbeSub, name: &str) -> Result<Option<ProbeToServer>, IpcError> {
    match sub
        .receive()
        .map_err(|e| IpcError::Message(format!("{name} receive: {e:?}")))?
    {
        Some(sample) => Ok(Some(*sample)),
        None => Ok(None),
    }
}

impl ServerProbeInbox {
    pub fn create(node: &Node<ipc::Service>, cluster_key: &str) -> Result<Self, IpcError> {
        let catalog_name = names::probe_catalog_a2s(cluster_key);
        let sample_name = names::probe_a2s(cluster_key);
        Ok(Self {
            catalog: open_sub(node, &catalog_name, true, PROBE_CATALOG_A2S_BUFFER)?,
            samples: open_sub(node, &sample_name, true, PROBE_A2S_BUFFER)?,
        })
    }

    pub fn try_recv_catalog(&self) -> Result<Option<ProbeToServer>, IpcError> {
        recv_one(&self.catalog, "probe catalog a2s")
    }

    pub fn try_recv_sample(&self) -> Result<Option<ProbeToServer>, IpcError> {
        recv_one(&self.samples, "probe a2s")
    }
}

pub struct ArbiterProbeForward {
    catalog: ProbePub,
    samples: ProbePub,
}

impl ArbiterProbeForward {
    pub fn open(node: &Node<ipc::Service>, cluster_key: &str) -> Result<Self, IpcError> {
        let catalog_name = names::probe_catalog_a2s(cluster_key);
        let sample_name = names::probe_a2s(cluster_key);
        Ok(Self {
            catalog: open_pub(node, &catalog_name, false, PROBE_CATALOG_A2S_BUFFER)?,
            samples: open_pub(node, &sample_name, false, PROBE_A2S_BUFFER)?,
        })
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
        self.send(&self.catalog, sample)
    }

    pub fn publish_catalog_done(&self) -> Result<(), IpcError> {
        self.send(
            &self.catalog,
            ProbeToServer {
                probe_id: 0,
                board_hash: 0,
                virtual_time_ns: 0,
                value: 0,
                msg: MSG_CATALOG_END,
                wire_kind: 0,
                name_len: 0,
                _pad: [0; 3],
                name: [0; PROBE_NAME_CAP],
            },
        )
    }

    pub fn publish_sample(
        &self,
        probe_id: u64,
        virtual_time_ns: u64,
        wire_kind: u8,
        value: u16,
    ) -> Result<(), IpcError> {
        self.send(
            &self.samples,
            ProbeToServer {
                probe_id,
                board_hash: 0,
                virtual_time_ns,
                value,
                msg: MSG_SAMPLE,
                wire_kind,
                name_len: 0,
                _pad: [0; 3],
                name: [0; PROBE_NAME_CAP],
            },
        )
    }

    fn send(&self, port: &ProbePub, sample: ProbeToServer) -> Result<(), IpcError> {
        port.send_copy(sample)
            .map_err(|e| IpcError::Message(format!("probe a2s send: {e:?}")))?;
        Ok(())
    }
}
