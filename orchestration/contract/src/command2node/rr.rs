//! Command ↔ Node control request-response service (#147 / #153).
//!
//! Both sides open the service through [`open_node_command_service`] so the
//! QoS (one Node server, one Command client, one request in flight) cannot
//! drift between processes.

use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

use iceoryx2::port::client::Client;
use iceoryx2::port::server::Server;
use iceoryx2::prelude::*;
use iceoryx2::service::port_factory::request_response::PortFactory;

use super::{NodeCommand, NodeCommandResult};
use crate::id::{ClusterId, HashedId};
use crate::ipc::IpcError;
use crate::names::node_command_rr_path;

pub type NodeCommandService = PortFactory<ipc::Service, NodeCommand, (), NodeCommandResult, ()>;
pub type NodeCommandServer = Server<ipc::Service, NodeCommand, (), NodeCommandResult, ()>;
type NodeCommandClientPort = Client<ipc::Service, NodeCommand, (), NodeCommandResult, ()>;

/// Sleep between polls while waiting for a server or a response.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Open (or create) the RR service at [`node_command_rr_path`].
pub fn open_node_command_service(
    node: &Node<ipc::Service>,
    cluster: ClusterId,
    node_key: HashedId<u64>,
) -> Result<NodeCommandService, IpcError> {
    let name = node_command_rr_path(cluster, node_key);
    let service_name: ServiceName = name
        .as_str()
        .try_into()
        .map_err(|e| IpcError::new(format!("bad service name {name}: {e:?}")))?;
    node.service_builder(&service_name)
        .request_response::<NodeCommand, NodeCommandResult>()
        .max_servers(1)
        .max_clients(1)
        .max_active_requests_per_client(1)
        .max_response_buffer_size(1)
        .open_or_create()
        .map_err(|e| IpcError::new(format!("open {name}: {e:?}")))
}

/// Node side: the single server port for this node's control service.
pub fn create_node_command_server(
    node: &Node<ipc::Service>,
    cluster: ClusterId,
    node_key: HashedId<u64>,
) -> Result<NodeCommandServer, IpcError> {
    open_node_command_service(node, cluster, node_key)?
        .server_builder()
        .create()
        .map_err(|e| IpcError::new(format!("node command server {node_key}: {e:?}")))
}

/// Why [`NodeCommandClient::request`] produced no result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeRequestError {
    Ipc(IpcError),
    /// No Node server connected before the deadline.
    NoServer,
    /// The server accepted the request but did not answer before the deadline.
    Timeout,
    /// The server dropped the request without answering.
    Disconnected,
}

impl fmt::Display for NodeRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ipc(e) => write!(f, "node command ipc: {e}"),
            Self::NoServer => f.write_str("node command: no server connected"),
            Self::Timeout => f.write_str("node command: response timed out"),
            Self::Disconnected => f.write_str("node command: server dropped the request"),
        }
    }
}

impl std::error::Error for NodeRequestError {}

/// Command side: blocking client for one node's control service.
pub struct NodeCommandClient {
    client: NodeCommandClientPort,
}

impl NodeCommandClient {
    pub fn create(
        node: &Node<ipc::Service>,
        cluster: ClusterId,
        node_key: HashedId<u64>,
    ) -> Result<Self, IpcError> {
        let client = open_node_command_service(node, cluster, node_key)?
            .client_builder()
            .create()
            .map_err(|e| IpcError::new(format!("node command client {node_key}: {e:?}")))?;
        Ok(Self { client })
    }

    /// Send `command` and wait for the reply.
    ///
    /// While no server is connected the request is re-sent every poll until
    /// `timeout`, so a Node that is still starting up is not an error.
    pub fn request(
        &self,
        command: NodeCommand,
        timeout: Duration,
    ) -> Result<NodeCommandResult, NodeRequestError> {
        let deadline = Instant::now() + timeout;
        let pending = loop {
            let pending = self
                .client
                .send_copy(command)
                .map_err(|e| NodeRequestError::Ipc(IpcError::new(format!("send: {e:?}"))))?;
            if pending.number_of_server_connections() > 0 {
                break pending;
            }
            drop(pending);
            if Instant::now() >= deadline {
                return Err(NodeRequestError::NoServer);
            }
            thread::sleep(POLL_INTERVAL);
        };
        let receive = || {
            pending
                .receive()
                .map(|response| response.map(|r| *r))
                .map_err(|e| NodeRequestError::Ipc(IpcError::new(format!("receive: {e:?}"))))
        };
        loop {
            if let Some(response) = receive()? {
                return Ok(response);
            }
            if !pending.is_connected() {
                // The server may answer and drop the request between the two checks.
                return receive()?.ok_or(NodeRequestError::Disconnected);
            }
            if Instant::now() >= deadline {
                return Err(NodeRequestError::Timeout);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{create_iox_node, isolated_config};

    /// Service shared memory is keyed by name only; keep concurrent tests apart.
    fn unique_cluster(test: u32) -> ClusterId {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        ClusterId::new(std::process::id() ^ (test << 24), nanos)
    }

    #[test]
    fn request_without_server_reports_no_server() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = isolated_config(dir.path()).unwrap();
        let node = create_iox_node(&cfg, "rr-test-client").unwrap();
        let client = NodeCommandClient::create(&node, unique_cluster(1), HashedId::new(3)).unwrap();
        assert_eq!(
            client.request(NodeCommand::Start, Duration::from_millis(20)),
            Err(NodeRequestError::NoServer)
        );
    }

    #[test]
    fn request_round_trips_through_server() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = isolated_config(dir.path()).unwrap();
        let cluster = unique_cluster(2);
        let key = HashedId::new(3);

        let server_cfg = cfg.clone();
        let server = thread::spawn(move || {
            let node = create_iox_node(&server_cfg, "rr-test-server").unwrap();
            let server = create_node_command_server(&node, cluster, key).unwrap();
            loop {
                if let Some(request) = server.receive().unwrap() {
                    assert_eq!(*request.payload(), NodeCommand::Stop);
                    request.send_copy(NodeCommandResult::Success).unwrap();
                    return;
                }
                thread::sleep(POLL_INTERVAL);
            }
        });

        let node = create_iox_node(&cfg, "rr-test-client").unwrap();
        let client = NodeCommandClient::create(&node, cluster, key).unwrap();
        assert_eq!(
            client.request(NodeCommand::Stop, Duration::from_secs(5)),
            Ok(NodeCommandResult::Success)
        );
        server.join().unwrap();
    }
}
