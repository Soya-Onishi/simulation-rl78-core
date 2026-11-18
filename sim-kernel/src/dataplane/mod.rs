//! Board-edge dataplane: payload types, iceoryx open, and typed lanes.
//!
//! [`ExternPort::open`] consumes a board port and returns a [`Lane`] whose
//! [`RxLane`] / [`TxLane`] pump iceoryx samples through handles captured at open
//! time. Service names use `sim-cluster/{cluster_key}/edge/{edge_id}`.
//!
//! Opened lanes hold iceoryx handles that are not [`Send`] (`Rc` internally).

mod analog;
mod digital;
mod pulse;
mod scalar;
mod uart;

pub use analog::AnalogVoltage;
pub use digital::DigitalLevel;
pub use pulse::Pulse;
pub use scalar::Scalar;
pub use uart::{UartFrame, UartParity, UartStopBits};

use std::fmt;

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::{Node, ServiceName, ipc};
use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;

use crate::{OutPort, SourcePort};

/// One directed topology edge attached to a board endpoint.
#[derive(Clone, Debug)]
pub struct EdgeSpec {
    /// Full iceoryx service name (already includes cluster key and edge id).
    pub service_name: String,
    /// Per-edge SPSC: topology currently always supplies `1`.
    pub max_publishers: usize,
    /// Per-edge SPSC: topology currently always supplies `1`.
    pub max_subscribers: usize,
    pub ring_capacity: usize,
}

/// Inputs for [`ExternPort::open`].
pub struct OpenCtx<'a> {
    pub node: &'a Node<ipc::Service>,
    /// Edges attached to this endpoint (RX: inbound edges; TX: outbound).
    pub edges: Vec<EdgeSpec>,
}

/// Failures opening iceoryx ports or building lanes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenError {
    Message(String),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for OpenError {}

/// Failures while pumping or discarding lane traffic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PumpError {
    Message(String),
}

impl fmt::Display for PumpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for PumpError {}

/// Build the unified edge service name.
#[must_use]
pub fn edge_service_name(cluster_key: &str, edge_id_hex: &str) -> String {
    format!("sim-cluster/{cluster_key}/edge/{edge_id_hex}")
}

/// Payload type that can be carried on an iceoryx edge.
pub trait DataPlane: ZeroCopySend + Copy + Clone + Send + Sync + std::fmt::Debug + 'static {
    const DEFAULT_RING_CAPACITY: usize;
}

fn open_or_create_service<T: DataPlane>(
    ctx: &OpenCtx<'_>,
    edge: &EdgeSpec,
) -> Result<
    iceoryx2::service::port_factory::publish_subscribe::PortFactory<ipc::Service, T, ()>,
    OpenError,
> {
    let svc_name: ServiceName = edge.service_name.as_str().try_into().map_err(|e| {
        OpenError::Message(format!("bad service name {}: {e:?}", edge.service_name))
    })?;
    ctx.node
        .service_builder(&svc_name)
        .publish_subscribe::<T>()
        .max_publishers(edge.max_publishers.max(1))
        .max_subscribers(edge.max_subscribers.max(1))
        .max_nodes(16)
        .subscriber_max_buffer_size(edge.ring_capacity.max(2))
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| OpenError::Message(format!("open_or_create {}: {e:?}", edge.service_name)))
}

fn open_subscriber<T: DataPlane>(
    ctx: &OpenCtx<'_>,
    edge: &EdgeSpec,
) -> Result<Subscriber<ipc::Service, T, ()>, OpenError> {
    open_or_create_service::<T>(ctx, edge)?
        .subscriber_builder()
        .create()
        .map_err(|e| OpenError::Message(format!("subscriber {}: {e:?}", edge.service_name)))
}

fn open_publisher<T: DataPlane>(
    ctx: &OpenCtx<'_>,
    edge: &EdgeSpec,
) -> Result<Publisher<ipc::Service, T, ()>, OpenError> {
    open_or_create_service::<T>(ctx, edge)?
        .publisher_builder()
        .create()
        .map_err(|e| OpenError::Message(format!("publisher {}: {e:?}", edge.service_name)))
}

/// Board-edge port that can open iceoryx lanes.
///
/// [`std::any::Any`] enables [`crate::BoardPorts::receive`] to downcast
/// `&dyn ExternPort` to [`crate::InPort<T>`] after trait upcasting.
pub trait ExternPort: std::any::Any {
    /// `true` for [`crate::InPort`], `false` for [`crate::OutPort`].
    fn is_input(&self) -> bool;
    /// [`std::any::TypeId`] of the payload `T` on this port.
    fn payload_type_id(&self) -> std::any::TypeId;
    /// Clear TX pending queues (no-op for inputs).
    fn clear_pending(&self);
    fn open(self: Box<Self>, ctx: OpenCtx<'_>) -> Result<Lane, OpenError>;
}

/// Opened lane for one board endpoint.
pub enum Lane {
    Rx(RxLane),
    Tx(TxLane),
}

/// Type-erased RX lane (holds [`RxInner`]).
pub struct RxLane {
    inner: Box<dyn RxInner>,
}

impl RxLane {
    pub fn pump(&mut self) -> Result<(), PumpError> {
        self.inner.pump()
    }

    pub fn discard(&mut self) -> Result<(), PumpError> {
        self.inner.discard()
    }
}

/// Type-erased TX lane (holds [`TxInner`]).
pub struct TxLane {
    inner: Box<dyn TxInner>,
}

impl TxLane {
    pub fn pump(&mut self) -> Result<(), PumpError> {
        self.inner.pump()
    }

    pub fn discard(&mut self) -> Result<(), PumpError> {
        self.inner.discard()
    }
}

trait RxInner {
    fn pump(&mut self) -> Result<(), PumpError>;
    fn discard(&mut self) -> Result<(), PumpError>;
}

trait TxInner {
    fn pump(&mut self) -> Result<(), PumpError>;
    fn discard(&mut self) -> Result<(), PumpError>;
}

struct RxInnerImpl<T: DataPlane> {
    subs: Vec<Subscriber<ipc::Service, T, ()>>,
    source: SourcePort<T>,
}

impl<T: DataPlane> RxInner for RxInnerImpl<T> {
    fn pump(&mut self) -> Result<(), PumpError> {
        for sub in &self.subs {
            for _ in 0..64 {
                match sub
                    .receive()
                    .map_err(|e| PumpError::Message(format!("recv: {e:?}")))?
                {
                    Some(sample) => self.source.drive(*sample),
                    None => break,
                }
            }
        }
        Ok(())
    }

    fn discard(&mut self) -> Result<(), PumpError> {
        for sub in &self.subs {
            while sub
                .receive()
                .map_err(|e| PumpError::Message(format!("recv: {e:?}")))?
                .is_some()
            {}
        }
        Ok(())
    }
}

struct TxInnerImpl<T: DataPlane> {
    pubs: Vec<Publisher<ipc::Service, T, ()>>,
    out: OutPort<T>,
}

impl<T: DataPlane> TxInner for TxInnerImpl<T> {
    fn pump(&mut self) -> Result<(), PumpError> {
        for frame in self.out.drain_pending() {
            for puber in &self.pubs {
                // Drop on failure and continue. iceoryx SendError here is typically
                // publisher data-segment exhaustion, not host OOM; re-queuing would
                // stall TX or reorder samples.
                if let Err(err) = puber.send_copy(frame) {
                    log::warn!("dataplane tx send_copy: {err:?}");
                }
            }
        }
        Ok(())
    }

    fn discard(&mut self) -> Result<(), PumpError> {
        self.out.clear_pending();
        Ok(())
    }
}

impl<T: DataPlane> ExternPort for crate::InPort<T> {
    fn is_input(&self) -> bool {
        true
    }

    fn payload_type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<T>()
    }

    fn clear_pending(&self) {}

    fn open(self: Box<Self>, ctx: OpenCtx<'_>) -> Result<Lane, OpenError> {
        if ctx.edges.is_empty() {
            return Err(OpenError::Message(format!(
                "no inbound edges for in-port `{}`",
                crate::InPort::name(&self)
            )));
        }
        let source = self.port().clone();
        let mut subs = Vec::with_capacity(ctx.edges.len());
        for edge in &ctx.edges {
            subs.push(open_subscriber::<T>(&ctx, edge)?);
        }
        Ok(Lane::Rx(RxLane {
            inner: Box::new(RxInnerImpl { subs, source }),
        }))
    }
}

impl<T: DataPlane> ExternPort for crate::OutPort<T> {
    fn is_input(&self) -> bool {
        false
    }

    fn payload_type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<T>()
    }

    fn clear_pending(&self) {
        crate::OutPort::clear_pending(self);
    }

    fn open(self: Box<Self>, ctx: OpenCtx<'_>) -> Result<Lane, OpenError> {
        if ctx.edges.is_empty() {
            return Err(OpenError::Message(format!(
                "no outbound edges for out-port `{}`",
                crate::OutPort::name(&self)
            )));
        }
        let out = (*self).clone();
        let mut pubs = Vec::with_capacity(ctx.edges.len());
        for edge in &ctx.edges {
            pubs.push(open_publisher::<T>(&ctx, edge)?);
        }
        Ok(Lane::Tx(TxLane {
            inner: Box::new(TxInnerImpl { pubs, out }),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InPort, OutPort, Wire};
    use iceoryx2::config::Config;
    use iceoryx2::prelude::*;
    use iceoryx2_bb_system_types::path::Path as IoxPath;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_iox_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("sim-kernel-dataplane-{tag}-{nanos}"))
    }

    fn isolated_node(root: &std::path::Path, name: &str) -> Node<ipc::Service> {
        std::fs::create_dir_all(root).unwrap();
        let mut cfg = Config::default();
        let mut owned = root.to_str().unwrap().to_string();
        if !owned.ends_with('/') {
            owned.push('/');
        }
        let iox_path = IoxPath::new(owned.as_bytes()).unwrap();
        cfg.global.set_root_path(&iox_path);
        let node_name = NodeName::new(name).unwrap();
        NodeBuilder::new()
            .name(&node_name)
            .config(&cfg)
            .signal_handling_mode(SignalHandlingMode::Disabled)
            .create::<ipc::Service>()
            .unwrap()
    }

    fn edge(cluster: &str, id: &str, capacity: usize) -> EdgeSpec {
        EdgeSpec {
            service_name: edge_service_name(cluster, id),
            max_publishers: 1,
            max_subscribers: 1,
            ring_capacity: capacity,
        }
    }

    #[test]
    fn edge_service_name_format() {
        assert_eq!(
            edge_service_name("k", "deadbeef"),
            "sim-cluster/k/edge/deadbeef"
        );
    }

    #[test]
    fn scalar_tx_rx_roundtrip_and_discard() {
        let root = temp_iox_root("scalar-rt");
        let cluster = "c";
        let na = isolated_node(&root, "a");
        let nb = isolated_node(&root, "b");
        let spec = edge(cluster, "ab02", Scalar::DEFAULT_RING_CAPACITY);

        let tx_port = OutPort::<Scalar>::new("tx_out").unwrap();
        let tx_drive = tx_port.clone();
        let mut rx_port = InPort::<Scalar>::new("rx_in").unwrap();
        let tap = OutPort::<Scalar>::new("tap").unwrap();
        let _w = Wire::new().source(rx_port.port_mut()).sink({
            let tap = tap.clone();
            move |values, changed| tap.on_input(values, changed)
        });

        let Lane::Tx(mut tx) = Box::new(tx_port)
            .open(OpenCtx {
                node: &na,
                edges: vec![spec.clone()],
            })
            .unwrap()
        else {
            panic!("expected Tx");
        };
        let Lane::Rx(mut rx) = Box::new(rx_port)
            .open(OpenCtx {
                node: &nb,
                edges: vec![spec],
            })
            .unwrap()
        else {
            panic!("expected Rx");
        };

        tx_drive.on_input(&[Scalar(1.5)], 0);
        tx.pump().unwrap();
        rx.pump().unwrap();
        assert_eq!(tap.pending().last().copied(), Some(Scalar(1.5)));

        tx_drive.on_input(&[Scalar(9.0)], 0);
        tx.pump().unwrap();
        rx.discard().unwrap();
        rx.pump().unwrap();
        assert_eq!(tap.pending().last().copied(), Some(Scalar(1.5)));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn digital_uart_analog_pulse_open() {
        let root = temp_iox_root("kinds");
        let na = isolated_node(&root, "a");
        let nb = isolated_node(&root, "b");

        for (id, capacity, open_tx, open_rx) in [
            (
                "d1",
                DigitalLevel::DEFAULT_RING_CAPACITY,
                Box::new(OutPort::<DigitalLevel>::new("d_out").unwrap()) as Box<dyn ExternPort>,
                Box::new(InPort::<DigitalLevel>::new("d_in").unwrap()) as Box<dyn ExternPort>,
            ),
            (
                "u1",
                UartFrame::DEFAULT_RING_CAPACITY,
                Box::new(OutPort::<UartFrame>::new("u_out").unwrap()) as Box<dyn ExternPort>,
                Box::new(InPort::<UartFrame>::new("u_in").unwrap()) as Box<dyn ExternPort>,
            ),
            (
                "a1",
                AnalogVoltage::DEFAULT_RING_CAPACITY,
                Box::new(OutPort::<AnalogVoltage>::new("a_out").unwrap()) as Box<dyn ExternPort>,
                Box::new(InPort::<AnalogVoltage>::new("a_in").unwrap()) as Box<dyn ExternPort>,
            ),
            (
                "p1",
                Pulse::DEFAULT_RING_CAPACITY,
                Box::new(OutPort::<Pulse>::new("p_out").unwrap()) as Box<dyn ExternPort>,
                Box::new(InPort::<Pulse>::new("p_in").unwrap()) as Box<dyn ExternPort>,
            ),
        ] {
            assert!(!open_tx.is_input());
            assert!(open_rx.is_input());
            let spec = edge("c", id, capacity);
            assert!(matches!(
                open_tx.open(OpenCtx {
                    node: &na,
                    edges: vec![spec.clone()],
                }),
                Ok(Lane::Tx(_))
            ));
            assert!(matches!(
                open_rx.open(OpenCtx {
                    node: &nb,
                    edges: vec![spec],
                }),
                Ok(Lane::Rx(_))
            ));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
