//! Command thread: sessions, the node registration table, and the Command →
//! Node RR clients (#147 C). iceoryx ports are not `Send`, so every client
//! lives on this thread; REST handlers reach it through [`CommandRequest`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use contract::{
    ClusterId, CommandHostStatus, HashedId, HostCtrl, HostCtrlLease, IoxNode, NodeCommand,
    NodeCommandClient, NodeCommandResult, NodeConfig, NodeProcessStatus, NodeRequestError,
    OrchestratorReply, RestErrorCode, SessionsBody, create_iox_node, isolated_config,
    remove_dead_nodes,
};
use tokio::sync::oneshot;

use crate::build::{BuildClient, BuildOp};
use crate::endpoint::EndpointEvent;
use crate::host::{CommandConfig, CommandHostError, remove_session_dir};

/// How often an idle Command thread checks the shutdown flag.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// One Node of one session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct NodeRef {
    pub(crate) cluster: ClusterId,
    pub(crate) node_key: HashedId<u64>,
}

pub(crate) enum SessionOp {
    Start,
    Stop,
    Reset,
    Status,
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum NodeOp {
    Command(NodeCommand),
    BuildEnsure,
    BuildRestart,
    BuildTerminate,
    BuildStatus,
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum CommandOp {
    CreateSession(SessionsBody),
    Session(ClusterId, SessionOp),
    Node(NodeRef, NodeOp),
}

pub(crate) type CommandReply = Result<OrchestratorReply, CommandError>;

pub(crate) struct CommandRequest {
    pub(crate) op: CommandOp,
    pub(crate) reply: oneshot::Sender<CommandReply>,
}

/// A failed request; the router maps `code` to the HTTP status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandError {
    pub(crate) code: RestErrorCode,
    pub(crate) message: String,
}

impl CommandError {
    pub(crate) fn new(code: RestErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(RestErrorCode::Internal, message)
    }

    fn node(node_key: HashedId<u64>, e: NodeRequestError) -> Self {
        let code = match e {
            NodeRequestError::Ipc(_) => RestErrorCode::Internal,
            NodeRequestError::NoServer
            | NodeRequestError::Timeout
            | NodeRequestError::Disconnected => RestErrorCode::Unavailable,
        };
        Self::new(code, format!("node {node_key}: {e}"))
    }
}

/// Serve requests until shutdown is requested, then terminate every session.
pub(crate) fn command_loop(
    config: &CommandConfig,
    requests: &Receiver<CommandRequest>,
    build: &BuildClient,
    endpoint: &Sender<EndpointEvent>,
    shutdown: &AtomicBool,
) -> Result<(), CommandHostError> {
    let mut state = CommandState {
        config,
        build,
        endpoint,
        sessions: HashMap::new(),
    };
    while !shutdown.load(Ordering::Acquire) {
        match requests.recv_timeout(SHUTDOWN_POLL) {
            Ok(request) => {
                let _ = request.reply.send(state.handle(request.op));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    state.close_all()
}

struct CommandState<'a> {
    config: &'a CommandConfig,
    build: &'a BuildClient,
    endpoint: &'a Sender<EndpointEvent>,
    sessions: HashMap<ClusterId, Session>,
}

enum RunState {
    Idle,
    /// Started while `host_ctrl.stop_generation` was this value.
    Running {
        stop_generation: u64,
    },
}

struct Session {
    cluster: ClusterId,
    iox_root: PathBuf,
    /// Boards this host launches (from [`SessionsBody::nodes`]).
    nodes: HashSet<HashedId<u64>>,
    /// Registration table: Nodes whose `build_ensure` succeeded.
    clients: HashMap<HashedId<u64>, NodeCommandClient>,
    run: RunState,
    // Dropped after `clients`: ports before their iceoryx node.
    iox: IoxNode,
    host_ctrl: HostCtrl,
    _host_ctrl_lease: HostCtrlLease,
}

impl CommandState<'_> {
    fn handle(&mut self, op: CommandOp) -> CommandReply {
        match op {
            CommandOp::CreateSession(body) => self.create_session(&body),
            CommandOp::Session(cluster, op) => self.session_op(cluster, op),
            CommandOp::Node(node, op) => self.node_op(node, op),
        }
    }

    fn create_session(&mut self, body: &SessionsBody) -> CommandReply {
        body.validate()
            .map_err(|e| CommandError::new(RestErrorCode::BadRequest, e.0))?;
        let board_count = u32::try_from(body.topology.board_count())
            .map_err(|_| CommandError::new(RestErrorCode::BadRequest, "too many boards"))?;
        let cluster = loop {
            let cluster = ClusterId::generate();
            if !self.sessions.contains_key(&cluster) {
                break cluster;
            }
        };
        let iox_root = self.config.session_root(cluster);
        let session =
            Session::open(cluster, iox_root.clone(), body, board_count).inspect_err(|_| {
                let _ = remove_session_dir(&iox_root);
            })?;
        self.sessions.insert(cluster, session);
        let _ = self
            .endpoint
            .send(EndpointEvent::SessionOpened { cluster, iox_root });
        Ok(OrchestratorReply::SessionCreated {
            cluster_id: cluster,
        })
    }

    fn session(&mut self, cluster: ClusterId) -> Result<&mut Session, CommandError> {
        self.sessions.get_mut(&cluster).ok_or_else(|| {
            CommandError::new(RestErrorCode::NotFound, format!("no session {cluster}"))
        })
    }

    fn session_op(&mut self, cluster: ClusterId, op: SessionOp) -> CommandReply {
        let timeout = self.config.node_timeout;
        let session = self.session(cluster)?;
        match op {
            SessionOp::Status => return Ok(OrchestratorReply::Status(session.status())),
            SessionOp::Start => {
                if let Some(node) = session
                    .nodes
                    .iter()
                    .find(|node| !session.clients.contains_key(node))
                {
                    return Err(CommandError::new(
                        RestErrorCode::Conflict,
                        format!("node {node} is not built"),
                    ));
                }
                // Read first: a bump during Start must still count as a stop.
                let stop_generation = session.host_ctrl.stop_generation();
                session.broadcast(NodeCommand::Start, timeout)?;
                session.run = RunState::Running { stop_generation };
            }
            SessionOp::Stop | SessionOp::Reset => {
                // Idle even if some Node fails: the others have already stopped.
                session.run = RunState::Idle;
                let command = match op {
                    SessionOp::Stop => NodeCommand::Stop,
                    _ => NodeCommand::Reset,
                };
                session.broadcast(command, timeout)?;
            }
        }
        Ok(OrchestratorReply::Ack)
    }

    fn node_op(&mut self, node: NodeRef, op: NodeOp) -> CommandReply {
        let timeout = self.config.node_timeout;
        let build = self.build;
        let session = self.session(node.cluster)?;
        if !session.nodes.contains(&node.node_key) {
            return Err(CommandError::new(
                RestErrorCode::NotFound,
                format!("node {} is not in session {}", node.node_key, node.cluster),
            ));
        }
        let call = |op| {
            build
                .call(op)
                .map_err(|e| CommandError::internal(format!("node {}: {e}", node.node_key)))
        };
        match op {
            NodeOp::Command(command) => {
                let client = session.clients.get(&node.node_key).ok_or_else(|| {
                    CommandError::new(
                        RestErrorCode::NotFound,
                        format!("node {} is not built", node.node_key),
                    )
                })?;
                let result = client
                    .request(command, timeout)
                    .map_err(|e| CommandError::node(node.node_key, e))?;
                return Ok(OrchestratorReply::Node(result.into()));
            }
            NodeOp::BuildEnsure => {
                call(BuildOp::Ensure(session.node_config(node.node_key)))?;
                session.register(node.node_key)?;
            }
            NodeOp::BuildRestart => {
                session.shutdown_node(node.node_key, build, timeout);
                call(BuildOp::Restart(session.node_config(node.node_key)))?;
                session.register(node.node_key)?;
            }
            NodeOp::BuildTerminate => {
                session.shutdown_node(node.node_key, build, timeout);
                session.clients.remove(&node.node_key);
                call(BuildOp::Terminate(node))?;
            }
            NodeOp::BuildStatus => {
                return Ok(OrchestratorReply::BuildStatus {
                    status: call(BuildOp::Status(node))?,
                });
            }
        }
        Ok(OrchestratorReply::Ack)
    }

    /// Shut down every Node, then remove each session directory.
    fn close_all(&mut self) -> Result<(), CommandHostError> {
        let mut first = Ok(());
        for (_, session) in self.sessions.drain() {
            let cluster = session.cluster;
            let result = session.close(self.build, self.config.node_timeout);
            let _ = self.endpoint.send(EndpointEvent::SessionClosed { cluster });
            if first.is_ok() {
                first = result;
            }
        }
        first
    }
}

impl Session {
    fn open(
        cluster: ClusterId,
        iox_root: PathBuf,
        body: &SessionsBody,
        board_count: u32,
    ) -> Result<Self, CommandError> {
        let host_ctrl = HostCtrl::create(&iox_root, board_count, body.has_remote_hosts())
            .map_err(|e| CommandError::internal(format!("host_ctrl: {e}")))?;
        let lease = HostCtrlLease::new(&iox_root);
        let iox = isolated_config(&iox_root)
            .and_then(|cfg| create_iox_node(&cfg, "sim-command"))
            .map_err(|e| CommandError::internal(e.to_string()))?;
        Ok(Self {
            cluster,
            iox_root,
            nodes: body.nodes.iter().copied().collect(),
            clients: HashMap::new(),
            run: RunState::Idle,
            iox,
            host_ctrl,
            _host_ctrl_lease: lease,
        })
    }

    /// Running until a Stop / Reset or a `host_ctrl` emergency stop.
    fn status(&self) -> CommandHostStatus {
        match self.run {
            RunState::Running { stop_generation }
                if self.host_ctrl.stop_generation() == stop_generation =>
            {
                CommandHostStatus::Running
            }
            _ => CommandHostStatus::Idle,
        }
    }

    fn node_config(&self, node_key: HashedId<u64>) -> NodeConfig {
        NodeConfig {
            iox_root: self.iox_root.clone(),
            cluster: self.cluster,
            node_key,
            board_count: self.host_ctrl.board_count(),
        }
    }

    /// Add `node_key` to the registration table after Build (re)launched it.
    fn register(&mut self, node_key: HashedId<u64>) -> Result<(), CommandError> {
        // A killed predecessor's server port stays registered until dead nodes
        // are removed, and the client would send to it instead of the new Node.
        isolated_config(&self.iox_root)
            .and_then(|config| remove_dead_nodes(&config))
            .map_err(|e| CommandError::internal(e.to_string()))?;
        if !self.clients.contains_key(&node_key) {
            let client = NodeCommandClient::create(&self.iox, self.cluster, node_key)
                .map_err(|e| CommandError::internal(e.to_string()))?;
            self.clients.insert(node_key, client);
        }
        Ok(())
    }

    /// Send `command` to every registered Node; report the first failure.
    fn broadcast(&self, command: NodeCommand, timeout: Duration) -> Result<(), CommandError> {
        let mut first = Ok(());
        for (&node_key, client) in &self.clients {
            let result = match client.request(command, timeout) {
                Ok(NodeCommandResult::Success) => Ok(()),
                Ok(other) => Err(CommandError::internal(format!(
                    "node {node_key}: {command:?} returned {other:?}"
                ))),
                Err(e) => Err(CommandError::node(node_key, e)),
            };
            if first.is_ok() {
                first = result;
            }
        }
        first
    }

    /// Ask a registered Node to exit unless Build reports it is not running;
    /// Build then waits for it.
    fn shutdown_node(&self, node_key: HashedId<u64>, build: &BuildClient, timeout: Duration) {
        let Some(client) = self.clients.get(&node_key) else {
            return;
        };
        let node = NodeRef {
            cluster: self.cluster,
            node_key,
        };
        // Still ask when the Build thread is gone: the Node may outlive it.
        let running = build.call(BuildOp::Status(node)).map_or(true, |process| {
            matches!(process, NodeProcessStatus::Running { .. })
        });
        if running {
            let _ = client.request(NodeCommand::Shutdown, timeout);
        }
    }

    fn close(self, build: &BuildClient, timeout: Duration) -> Result<(), CommandHostError> {
        let mut first = Ok(());
        for &node_key in &self.nodes {
            self.shutdown_node(node_key, build, timeout);
            let node = NodeRef {
                cluster: self.cluster,
                node_key,
            };
            if let (Err(e), Ok(())) = (build.call(BuildOp::Terminate(node)), &first) {
                first = Err(CommandHostError::Io(e));
            }
        }
        let iox_root = self.iox_root.clone();
        drop(self);
        first.and(remove_session_dir(&iox_root))
    }
}
