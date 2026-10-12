//! Per-probe iceoryx2 pub/sub sample types and port helpers.

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use sim_kernel::{DigitalLevel, UartFrame};

use super::names;
use super::runtime::IpcError;

/// Default subscriber buffer for a probe service.
pub const PROBE_BUFFER: usize = 64;

/// Digital probe sample on the wire (`value` is [`DigitalLevel`] as `u8`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub struct ProbeDigitalSample {
    pub probe_id: u64,
    pub virtual_time_ns: u64,
    pub value: u8,
}

impl ProbeDigitalSample {
    #[must_use]
    pub fn new(probe_id: u64, virtual_time_ns: u64, value: DigitalLevel) -> Self {
        Self {
            probe_id,
            virtual_time_ns,
            value: value as u8,
        }
    }

    #[must_use]
    pub fn level(self) -> Option<DigitalLevel> {
        match self.value {
            0 => Some(DigitalLevel::HiZ),
            1 => Some(DigitalLevel::High),
            2 => Some(DigitalLevel::Low),
            _ => None,
        }
    }
}

/// UART probe sample on the wire.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub struct ProbeUartSample {
    pub probe_id: u64,
    pub virtual_time_ns: u64,
    pub value: UartFrame,
}

pub type ProbeDigitalPub = Publisher<ipc::Service, ProbeDigitalSample, ()>;
pub type ProbeDigitalSub = Subscriber<ipc::Service, ProbeDigitalSample, ()>;
pub type ProbeUartPub = Publisher<ipc::Service, ProbeUartSample, ()>;
pub type ProbeUartSub = Subscriber<ipc::Service, ProbeUartSample, ()>;

/// Open or create a digital probe publisher for `(board_hash, name_hash)`.
pub fn open_digital_publisher(
    node: &Node<ipc::Service>,
    cluster_key: &str,
    board_hash: u64,
    name_hash: u64,
) -> Result<ProbeDigitalPub, IpcError> {
    let name = names::probe(cluster_key, board_hash, name_hash);
    open_digital_publisher_named(node, &name, PROBE_BUFFER)
}

/// Open or create a UART probe publisher for `(board_hash, name_hash)`.
pub fn open_uart_publisher(
    node: &Node<ipc::Service>,
    cluster_key: &str,
    board_hash: u64,
    name_hash: u64,
) -> Result<ProbeUartPub, IpcError> {
    let name = names::probe(cluster_key, board_hash, name_hash);
    open_uart_publisher_named(node, &name, PROBE_BUFFER)
}

/// Open or create a digital probe subscriber.
pub fn open_digital_subscriber(
    node: &Node<ipc::Service>,
    cluster_key: &str,
    board_hash: u64,
    name_hash: u64,
) -> Result<ProbeDigitalSub, IpcError> {
    let name = names::probe(cluster_key, board_hash, name_hash);
    open_digital_subscriber_named(node, &name, PROBE_BUFFER)
}

/// Open or create a UART probe subscriber.
pub fn open_uart_subscriber(
    node: &Node<ipc::Service>,
    cluster_key: &str,
    board_hash: u64,
    name_hash: u64,
) -> Result<ProbeUartSub, IpcError> {
    let name = names::probe(cluster_key, board_hash, name_hash);
    open_uart_subscriber_named(node, &name, PROBE_BUFFER)
}

fn open_digital_publisher_named(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<ProbeDigitalPub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<ProbeDigitalSample>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create probe pub {name}: {e:?}")))?;
    svc.publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("probe publisher {name}: {e:?}")))
}

fn open_uart_publisher_named(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<ProbeUartPub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<ProbeUartSample>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create probe pub {name}: {e:?}")))?;
    svc.publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("probe publisher {name}: {e:?}")))
}

fn open_digital_subscriber_named(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<ProbeDigitalSub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<ProbeDigitalSample>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create probe sub {name}: {e:?}")))?;
    svc.subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("probe subscriber {name}: {e:?}")))
}

fn open_uart_subscriber_named(
    node: &Node<ipc::Service>,
    name: &str,
    capacity: usize,
) -> Result<ProbeUartSub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<ProbeUartSample>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(16)
        .subscriber_max_buffer_size(capacity)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create probe sub {name}: {e:?}")))?;
    svc.subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("probe subscriber {name}: {e:?}")))
}

/// Best-effort publish; loan/send failure drops the sample.
pub fn send_digital(puber: &ProbeDigitalPub, sample: ProbeDigitalSample) {
    if let Err(e) = puber.send_copy(sample) {
        log::warn!("probe digital send dropped: {e:?}");
    }
}

/// Best-effort publish; loan/send failure drops the sample.
pub fn send_uart(puber: &ProbeUartPub, sample: ProbeUartSample) {
    if let Err(e) = puber.send_copy(sample) {
        log::warn!("probe uart send dropped: {e:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::hash::{board_id_hash, probe_name_hash};
    use crate::ipc::runtime::{create_node, isolated_config, new_cluster_key};
    use std::time::Duration;

    #[test]
    fn digital_probe_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = new_cluster_key();
        let root = dir.path().join("iox");
        let config = isolated_config(&root).unwrap();
        let na = create_node(&config, "probe-a").unwrap();
        let nb = create_node(&config, "probe-b").unwrap();
        let board_hash = board_id_hash("board0");
        let name_hash = probe_name_hash("vcc");
        let puber = open_digital_publisher(&na, &key, board_hash, name_hash).unwrap();
        let sub = open_digital_subscriber(&nb, &key, board_hash, name_hash).unwrap();
        let sample = ProbeDigitalSample::new(1, 99, DigitalLevel::High);
        send_digital(&puber, sample);
        let mut got = None;
        for _ in 0..100 {
            if let Ok(Some(s)) = sub.receive() {
                got = Some(*s);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, Some(sample));
        assert_eq!(
            got.and_then(ProbeDigitalSample::level),
            Some(DigitalLevel::High)
        );
    }
}
