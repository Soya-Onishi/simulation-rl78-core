//! Action pub/sub, separate from the control-plane samples.
//!
//! `n2a` / `a2s` carry advertisements and results. `s2a` / `a2n` carry requests.
//! Node request subscribers use [`crate::action::ACTION_RECV_CAP`] slots.

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::action::{ACTION_RECV_CAP, ActionRequest, ActionUpstream};

use super::names;
use super::runtime::IpcError;

type UpPub = Publisher<ipc::Service, ActionUpstream, ()>;
type UpSub = Subscriber<ipc::Service, ActionUpstream, ()>;
type ReqPub = Publisher<ipc::Service, ActionRequest, ()>;
type ReqSub = Subscriber<ipc::Service, ActionRequest, ()>;

fn svc_name(name: &str) -> Result<ServiceName, IpcError> {
    name.try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))
}

fn qos<T: ZeroCopySend + std::fmt::Debug>(
    node: &Node<ipc::Service>,
    name: &str,
    publishers: usize,
    subscribers: usize,
    nodes: usize,
    buffer: usize,
    create: bool,
) -> Result<
    iceoryx2::service::port_factory::publish_subscribe::PortFactory<ipc::Service, T, ()>,
    IpcError,
> {
    let builder = node
        .service_builder(&svc_name(name)?)
        .publish_subscribe::<T>()
        .max_publishers(publishers.max(1))
        .max_subscribers(subscribers.max(1))
        .max_nodes(nodes.max(1))
        .subscriber_max_buffer_size(buffer.max(1))
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

fn publisher<T: ZeroCopySend + std::fmt::Debug>(
    factory: iceoryx2::service::port_factory::publish_subscribe::PortFactory<ipc::Service, T, ()>,
    name: &str,
) -> Result<Publisher<ipc::Service, T, ()>, IpcError> {
    factory
        .publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("publisher {name}: {e:?}")))
}

fn subscriber<T: ZeroCopySend + std::fmt::Debug>(
    factory: iceoryx2::service::port_factory::publish_subscribe::PortFactory<ipc::Service, T, ()>,
    name: &str,
) -> Result<Subscriber<ipc::Service, T, ()>, IpcError> {
    factory
        .subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("subscriber {name}: {e:?}")))
}

fn node_slots(board_count: usize) -> usize {
    board_count.max(1) + 4
}

fn upstream_buffer(board_count: usize) -> usize {
    board_count.max(1).saturating_mul(ACTION_RECV_CAP)
}

/// Arbiter side of the node action channels (`n2a` subscriber, `a2n` publisher).
pub struct ArbiterNodeActions {
    n2a: UpSub,
    a2n: ReqPub,
}

impl ArbiterNodeActions {
    pub fn create(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_count: usize,
    ) -> Result<Self, IpcError> {
        let n2a_name = names::action_n2a(cluster_key);
        let a2n_name = names::action_a2n(cluster_key);
        let slots = node_slots(board_count);
        let n2a = qos::<ActionUpstream>(
            node,
            &n2a_name,
            board_count,
            1,
            slots,
            upstream_buffer(board_count),
            true,
        )?;
        let a2n = qos::<ActionRequest>(
            node,
            &a2n_name,
            1,
            board_count,
            slots,
            ACTION_RECV_CAP,
            true,
        )?;
        Ok(Self {
            n2a: subscriber(n2a, &n2a_name)?,
            a2n: publisher(a2n, &a2n_name)?,
        })
    }

    pub fn try_upstream(&self) -> Result<Option<ActionUpstream>, IpcError> {
        recv(&self.n2a, "action n2a")
    }

    pub fn publish_request(&self, request: &ActionRequest) -> Result<(), IpcError> {
        self.a2n
            .send_copy(*request)
            .map_err(|e| IpcError::Message(format!("action a2n send: {e:?}")))?;
        Ok(())
    }
}

/// Node side of the action channels.
pub struct NodeActions {
    n2a: UpPub,
    a2n: ReqSub,
}

impl NodeActions {
    pub fn open(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_count: usize,
    ) -> Result<Self, IpcError> {
        let n2a_name = names::action_n2a(cluster_key);
        let a2n_name = names::action_a2n(cluster_key);
        let slots = node_slots(board_count);
        let n2a = qos::<ActionUpstream>(
            node,
            &n2a_name,
            board_count,
            1,
            slots,
            upstream_buffer(board_count),
            false,
        )?;
        let a2n = qos::<ActionRequest>(
            node,
            &a2n_name,
            1,
            board_count,
            slots,
            ACTION_RECV_CAP,
            false,
        )?;
        Ok(Self {
            n2a: publisher(n2a, &n2a_name)?,
            a2n: subscriber(a2n, &a2n_name)?,
        })
    }

    pub fn publish_upstream(&self, msg: &ActionUpstream) -> Result<(), IpcError> {
        self.n2a
            .send_copy(*msg)
            .map_err(|e| IpcError::Message(format!("action n2a send: {e:?}")))?;
        Ok(())
    }

    pub fn try_request(&self) -> Result<Option<ActionRequest>, IpcError> {
        recv(&self.a2n, "action a2n")
    }
}

fn recv<T: Copy + std::fmt::Debug + ZeroCopySend>(
    sub: &Subscriber<ipc::Service, T, ()>,
    what: &str,
) -> Result<Option<T>, IpcError> {
    match sub
        .receive()
        .map_err(|e| IpcError::Message(format!("{what} receive: {e:?}")))?
    {
        Some(sample) => Ok(Some(*sample)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{ActionResult, ActionStatus};
    use crate::ipc::runtime::{create_node, isolated_config};
    use crate::ipc::server_arbiter;

    #[test]
    fn action_request_round_trips_server_arbiter_node() {
        let dir = tempfile::tempdir().unwrap();
        let key = format!("act-{}", std::process::id());
        let config = isolated_config(&dir.path().join("iox")).unwrap();
        let (server_link, arb_link) = server_arbiter::open_pair();
        let arb_node = create_node(&config, "act-arb").unwrap();
        let arb_nodes = ArbiterNodeActions::create(&arb_node, &key, 1).unwrap();
        let node = create_node(&config, "act-node").unwrap();
        let node_actions = NodeActions::open(&node, &key, 1).unwrap();

        let request = ActionRequest {
            request_id: 7,
            board_hash: crate::ipc::HashedId::new(3),
            device_hash: crate::ipc::HashedId::new(4),
            action_hash: crate::ipc::HashedId::new(5),
            ..ActionRequest::default()
        };
        server_link.actions.publish_request(&request).unwrap();
        let forwarded = arb_link.server_actions.try_request().unwrap().expect("s2a");
        assert_eq!(forwarded.request_id, 7);
        arb_nodes.publish_request(&forwarded).unwrap();
        let at_node = node_actions.try_request().unwrap().expect("a2n");
        assert_eq!(at_node.action_hash, crate::ipc::HashedId::new(5));
        node_actions
            .publish_upstream(&ActionUpstream::Result(ActionResult {
                request_id: 7,
                status: ActionStatus::Applied as u8,
            }))
            .unwrap();
        let upstream = arb_nodes.try_upstream().unwrap().expect("n2a");
        arb_link.server_actions.publish_upstream(&upstream).unwrap();
        match server_link.actions.try_upstream().unwrap().expect("a2s") {
            ActionUpstream::Result(result) => {
                assert_eq!(result.request_id, 7);
                assert_eq!(result.status, ActionStatus::Applied as u8);
            }
            ActionUpstream::Advertise(_) | ActionUpstream::CatalogEnd => {
                panic!("expected a result")
            }
        }
    }
}
