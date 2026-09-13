//! Arbiter/node control pubsub (a2n / n2a).

use std::collections::HashMap;

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::control::{BoardTarget, ControlToArbiter, ControlToNode};

use super::names;
use super::runtime::IpcError;

type A2nPub = Publisher<ipc::Service, ControlToNode, ()>;
type A2nSub = Subscriber<ipc::Service, ControlToNode, ()>;
type N2aPub = Publisher<ipc::Service, ControlToArbiter, ()>;
type N2aSub = Subscriber<ipc::Service, ControlToArbiter, ()>;

/// Arbiter-side control ports (owns create).
pub struct ArbiterControl {
    pub a2n_pub: A2nPub,
    pub n2a_sub: N2aSub,
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
                &a2n_name.as_str().try_into().map_err(|e| {
                    IpcError::Message(format!("bad service name {a2n_name}: {e:?}"))
                })?,
            )
            .publish_subscribe::<ControlToNode>()
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
                &n2a_name.as_str().try_into().map_err(|e| {
                    IpcError::Message(format!("bad service name {n2a_name}: {e:?}"))
                })?,
            )
            .publish_subscribe::<ControlToArbiter>()
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

    /// Resolve a wire board hash to its topology id.
    #[must_use]
    pub fn resolve_board(&self, board_id_hash: u64) -> Option<&str> {
        self.boards.get(&board_id_hash).map(String::as_str)
    }

    pub fn publish(&self, msg: &ControlToNode) -> Result<(), IpcError> {
        self.a2n_pub
            .send_copy(*msg)
            .map_err(|e| IpcError::Message(format!("a2n send: {e:?}")))?;
        Ok(())
    }

    pub fn try_recv(&self) -> Result<Option<ControlToArbiter>, IpcError> {
        match self
            .n2a_sub
            .receive()
            .map_err(|e| IpcError::Message(format!("n2a receive: {e:?}")))?
        {
            Some(sample) => {
                let msg: ControlToArbiter = *sample;
                match filter_known_sender(&self.boards, msg) {
                    Some(msg) => Ok(Some(msg)),
                    None => Err(IpcError::Message(format!(
                        "n2a unknown board hash in message={msg:?}"
                    ))),
                }
            }
            None => Ok(None),
        }
    }
}

/// Node-side control ports (`open_or_create` with the same QoS as arbiter create).
pub struct NodeControl {
    pub a2n_sub: A2nSub,
    pub n2a_pub: N2aPub,
    boards: HashMap<u64, String>,
}

impl NodeControl {
    pub fn open(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        boards: HashMap<u64, String>,
    ) -> Result<Self, IpcError> {
        let max_nodes = boards.len().max(1);
        let a2n_name = names::ctrl_a2n(cluster_key);
        let n2a_name = names::ctrl_n2a(cluster_key);

        let a2n_sub = open_or_create_a2n_subscriber(node, &a2n_name, max_nodes)?;
        let n2a_pub = open_or_create_n2a_publisher(node, &n2a_name, max_nodes)?;

        Ok(Self {
            a2n_sub,
            n2a_pub,
            boards,
        })
    }

    pub fn publish(&self, msg: &ControlToArbiter) -> Result<(), IpcError> {
        self.n2a_pub
            .send_copy(*msg)
            .map_err(|e| IpcError::Message(format!("n2a send: {e:?}")))?;
        Ok(())
    }

    pub fn try_recv(&self) -> Result<Option<ControlToNode>, IpcError> {
        match self
            .a2n_sub
            .receive()
            .map_err(|e| IpcError::Message(format!("a2n receive: {e:?}")))?
        {
            Some(sample) => {
                let msg: ControlToNode = *sample;
                // Unknown Unicast destinations are ignored quietly.
                Ok(filter_known_destination(&self.boards, msg))
            }
            None => Ok(None),
        }
    }
}

/// Drop a2n messages whose Unicast destination is not in the local board table.
fn filter_known_destination(
    boards: &HashMap<u64, String>,
    msg: ControlToNode,
) -> Option<ControlToNode> {
    match msg.destination() {
        BoardTarget::Broadcast => Some(msg),
        BoardTarget::Unicast { board_id_hash } if boards.contains_key(&board_id_hash) => Some(msg),
        BoardTarget::Unicast { .. } => None,
    }
}

/// Drop n2a messages whose sender hash is not in the local board table.
fn filter_known_sender(
    boards: &HashMap<u64, String>,
    msg: ControlToArbiter,
) -> Option<ControlToArbiter> {
    if boards.contains_key(&msg.from_board()) {
        Some(msg)
    } else {
        None
    }
}

fn open_or_create_a2n_subscriber(
    node: &Node<ipc::Service>,
    name: &str,
    max_nodes: usize,
) -> Result<A2nSub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<ControlToNode>()
        .max_publishers(1)
        .max_subscribers(max_nodes)
        .max_nodes(max_nodes + 4)
        .subscriber_max_buffer_size(32)
        .history_size(16)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create a2n subscriber {name}: {e:?}")))?;
    svc.subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("a2n subscriber {name}: {e:?}")))
}

fn open_or_create_n2a_publisher(
    node: &Node<ipc::Service>,
    name: &str,
    max_nodes: usize,
) -> Result<N2aPub, IpcError> {
    let svc_name: ServiceName = name
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<ControlToArbiter>()
        .max_publishers(max_nodes)
        .max_subscribers(1)
        .max_nodes(max_nodes + 4)
        .subscriber_max_buffer_size(64)
        .history_size(16)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| IpcError::Message(format!("open_or_create n2a publisher {name}: {e:?}")))?;
    svc.publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("n2a publisher {name}: {e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::hash::{board_hash_table, board_id_hash};
    use crate::ipc::runtime::{create_node, isolated_config};
    use std::time::Duration;

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
        let nctl = NodeControl::open(&node, &key, boards).unwrap();

        let hash_a = board_id_hash("a");
        arb.publish(&ControlToNode::StartupRecord {
            target: BoardTarget::Broadcast,
            margin_ns: 1,
            headroom_threshold_ns: 1,
        })
        .unwrap();

        let mut got_startup = false;
        for _ in 0..200 {
            if let Some(ControlToNode::StartupRecord { target, .. }) = nctl.try_recv().unwrap()
                && target == BoardTarget::Broadcast
            {
                got_startup = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(got_startup, "node did not receive StartupRecord");

        nctl.publish(&ControlToArbiter::Ready { from: hash_a })
            .unwrap();

        let mut got_ready = false;
        for _ in 0..200 {
            if let Some(ControlToArbiter::Ready { from }) = arb.try_recv().unwrap()
                && from == hash_a
            {
                got_ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(got_ready, "arbiter did not receive Ready");
    }
}
