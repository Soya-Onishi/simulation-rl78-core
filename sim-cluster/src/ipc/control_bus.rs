//! Arbiter/node control pubsub (a2n / n2a).

use std::collections::HashMap;
use std::fmt::Debug;
use std::time::{Duration, Instant};

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::control::ControlMessage;

use super::names;
use super::runtime::IpcError;
use super::wire::ControlWire;

type CtrlPub = Publisher<ipc::Service, ControlWire, ()>;
type CtrlSub = Subscriber<ipc::Service, ControlWire, ()>;

/// Arbiter-side control ports (owns create).
pub struct ArbiterControl {
    pub a2n_pub: CtrlPub,
    pub n2a_sub: CtrlSub,
    boards: HashMap<u64, String>,
}

impl ArbiterControl {
    pub fn create(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_count: usize,
        boards: HashMap<u64, String>,
    ) -> Result<Self, IpcError> {
        let max_nodes = board_count.max(1);
        let a2n_name = names::ctrl_a2n(cluster_key);
        let n2a_name = names::ctrl_n2a(cluster_key);

        let a2n = node
            .service_builder(
                &a2n_name
                    .as_str()
                    .try_into()
                    .map_err(|e| IpcError::Message(format!("bad service name {a2n_name}: {e:?}")))?,
            )
            .publish_subscribe::<ControlWire>()
            .max_publishers(1)
            .max_subscribers(max_nodes)
            .max_nodes(max_nodes + 4)
            .subscriber_max_buffer_size(32)
            .history_size(16)
            .enable_safe_overflow(true)
            .create()
            .map_err(|e| IpcError::Message(format!("create a2n failed: {e:?}")))?;

        let n2a = node
            .service_builder(
                &n2a_name
                    .as_str()
                    .try_into()
                    .map_err(|e| IpcError::Message(format!("bad service name {n2a_name}: {e:?}")))?,
            )
            .publish_subscribe::<ControlWire>()
            .max_publishers(max_nodes)
            .max_subscribers(1)
            .max_nodes(max_nodes + 4)
            .subscriber_max_buffer_size(64)
            .history_size(16)
            .enable_safe_overflow(true)
            .create()
            .map_err(|e| IpcError::Message(format!("create n2a failed: {e:?}")))?;

        let a2n_pub = a2n
            .publisher_builder()
            .create()
            .map_err(|e| IpcError::Message(format!("a2n publisher: {e:?}")))?;
        let n2a_sub = n2a
            .subscriber_builder()
            .create()
            .map_err(|e| IpcError::Message(format!("n2a subscriber: {e:?}")))?;

        Ok(Self {
            a2n_pub,
            n2a_sub,
            boards,
        })
    }

    pub fn publish(&self, msg: &ControlMessage) -> Result<(), IpcError> {
        let wire = ControlWire::from_logical(msg)
            .ok_or_else(|| IpcError::Message(format!("cannot encode control message: {msg:?}")))?;
        self.a2n_pub
            .send_copy(wire)
            .map_err(|e| IpcError::Message(format!("a2n send: {e:?}")))?;
        Ok(())
    }

    pub fn try_recv(&self) -> Result<Option<ControlMessage>, IpcError> {
        match self
            .n2a_sub
            .receive()
            .map_err(|e| IpcError::Message(format!("n2a receive: {e:?}")))?
        {
            Some(sample) => {
                let wire: ControlWire = *sample;
                wire.to_logical(&self.boards).map(Some).ok_or_else(|| {
                    IpcError::Message(format!("n2a decode failed for wire={wire:?}"))
                })
            }
            None => Ok(None),
        }
    }
}

/// Node-side control ports (opens existing services).
pub struct NodeControl {
    pub a2n_sub: CtrlSub,
    pub n2a_pub: CtrlPub,
    boards: HashMap<u64, String>,
}

impl NodeControl {
    pub fn open(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        boards: HashMap<u64, String>,
        open_budget: Duration,
    ) -> Result<Self, IpcError> {
        let a2n_name = names::ctrl_a2n(cluster_key);
        let n2a_name = names::ctrl_n2a(cluster_key);
        let deadline = Instant::now() + open_budget;

        let a2n_sub = open_subscriber::<ControlWire>(node, &a2n_name, deadline)?;
        let n2a_pub = open_publisher::<ControlWire>(node, &n2a_name, deadline)?;

        Ok(Self {
            a2n_sub,
            n2a_pub,
            boards,
        })
    }

    pub fn publish(&self, msg: &ControlMessage) -> Result<(), IpcError> {
        let wire = ControlWire::from_logical(msg)
            .ok_or_else(|| IpcError::Message(format!("cannot encode control message: {msg:?}")))?;
        self.n2a_pub
            .send_copy(wire)
            .map_err(|e| IpcError::Message(format!("n2a send: {e:?}")))?;
        Ok(())
    }

    pub fn try_recv(&self) -> Result<Option<ControlMessage>, IpcError> {
        match self
            .a2n_sub
            .receive()
            .map_err(|e| IpcError::Message(format!("a2n receive: {e:?}")))?
        {
            Some(sample) => {
                let wire: ControlWire = *sample;
                // Unknown board hashes (other boards' Startup) are ignored quietly.
                Ok(wire.to_logical(&self.boards))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::hash::board_hash_table;
    use crate::ipc::runtime::{create_node, isolated_config};

    #[test]
    fn control_round_trip_ready() {
        let dir = tempfile::tempdir().unwrap();
        let key = format!("rt-{}", std::process::id());
        let root = dir.path().join("iox");
        let boards = board_hash_table(["a", "b"]).unwrap();
        let config = isolated_config(&root).unwrap();
        let arb_node = create_node(&config, "rt-arb").unwrap();
        let arb = ArbiterControl::create(&arb_node, &key, 2, boards.clone()).unwrap();

        let node = create_node(&config, "rt-node-a").unwrap();
        let nctl = NodeControl::open(&node, &key, boards, Duration::from_secs(2)).unwrap();

        arb.publish(&ControlMessage::StartupRecord {
            board_id: "a".into(),
            margin_ns: 1,
            headroom_threshold_ns: 1,
        })
        .unwrap();

        let mut got_startup = false;
        for _ in 0..200 {
            if let Some(ControlMessage::StartupRecord { board_id, .. }) = nctl.try_recv().unwrap()
                && board_id == "a"
            {
                got_startup = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(got_startup, "node did not receive StartupRecord");

        nctl.publish(&ControlMessage::Ready {
            board_id: "a".into(),
        })
        .unwrap();

        let mut got_ready = false;
        for _ in 0..200 {
            if let Some(ControlMessage::Ready { board_id }) = arb.try_recv().unwrap()
                && board_id == "a"
            {
                got_ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(got_ready, "arbiter did not receive Ready");
    }
}

fn open_subscriber<T: Debug + ZeroCopySend + 'static>(
    node: &Node<ipc::Service>,
    name: &str,
    deadline: Instant,
) -> Result<Subscriber<ipc::Service, T, ()>, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    loop {
        match node
            .service_builder(&svc_name)
            .publish_subscribe::<T>()
            .open()
        {
            Ok(svc) => {
                return svc
                    .subscriber_builder()
                    .create()
                    .map_err(|e| IpcError::Message(format!("subscriber {name}: {e:?}")));
            }
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                return Err(IpcError::Message(format!("open subscriber {name}: {e:?}")));
            }
        }
    }
}

fn open_publisher<T: Debug + ZeroCopySend + 'static>(
    node: &Node<ipc::Service>,
    name: &str,
    deadline: Instant,
) -> Result<Publisher<ipc::Service, T, ()>, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    loop {
        match node
            .service_builder(&svc_name)
            .publish_subscribe::<T>()
            .open()
        {
            Ok(svc) => {
                return svc
                    .publisher_builder()
                    .create()
                    .map_err(|e| IpcError::Message(format!("publisher {name}: {e:?}")));
            }
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                return Err(IpcError::Message(format!("open publisher {name}: {e:?}")));
            }
        }
    }
}
