//! Build thread: Node process lifecycle only (#147 F). No build pipeline runs.
//!
//! Each Node starts in its own process group so a terminal Ctrl-C reaches only
//! Command, which then stops the Nodes with `NodeCommand::Shutdown`.

use std::collections::HashMap;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use contract::{NodeConfig, NodeProcessStatus};

use crate::command::NodeRef;
use crate::host::CommandConfig;

/// Poll period while waiting for a Node process to exit.
const EXIT_POLL: Duration = Duration::from_millis(10);

pub(crate) enum BuildOp {
    /// Start the Node unless it is already running.
    Ensure(NodeConfig),
    /// Wait for the running Node to exit (killing it after the timeout), then start it.
    Restart(NodeConfig),
    /// Wait for the Node to exit, killing it after the timeout.
    Terminate(NodeRef),
    Status(NodeRef),
}

pub(crate) struct BuildRequest {
    op: BuildOp,
    reply: SyncSender<io::Result<NodeProcessStatus>>,
}

/// Command-thread handle: one blocking request at a time.
pub(crate) struct BuildClient(Sender<BuildRequest>);

impl BuildClient {
    pub(crate) fn new(sender: Sender<BuildRequest>) -> Self {
        Self(sender)
    }

    pub(crate) fn call(&self, op: BuildOp) -> io::Result<NodeProcessStatus> {
        let (reply, reply_rx) = mpsc::sync_channel(1);
        let stopped = || io::Error::other("build thread stopped");
        self.0
            .send(BuildRequest { op, reply })
            .map_err(|_| stopped())?;
        reply_rx.recv().map_err(|_| stopped())?
    }
}

/// Serve requests until the Command thread drops its sender, then kill any
/// Node still running (Command has already asked them to shut down).
pub(crate) fn build_loop(config: &CommandConfig, requests: &Receiver<BuildRequest>) {
    let mut processes = Processes {
        config,
        children: HashMap::new(),
    };
    for request in requests {
        let _ = request.reply.send(processes.handle(request.op));
    }
    for child in processes.children.values_mut() {
        let _ = wait_or_kill(child, Duration::ZERO);
    }
}

/// Every Node process started in this Command host. Exited children stay so
/// `Status` can report their exit code.
struct Processes<'a> {
    config: &'a CommandConfig,
    children: HashMap<NodeRef, Child>,
}

impl Processes<'_> {
    fn handle(&mut self, op: BuildOp) -> io::Result<NodeProcessStatus> {
        match op {
            BuildOp::Ensure(node) => match self.children.get_mut(&node_ref(&node)).map(status) {
                Some(running @ NodeProcessStatus::Running { .. }) => Ok(running),
                _ => self.start(&node),
            },
            BuildOp::Restart(node) => {
                self.terminate(node_ref(&node))?;
                self.start(&node)
            }
            BuildOp::Terminate(node) => self.terminate(node),
            BuildOp::Status(node) => Ok(self
                .children
                .get_mut(&node)
                .map_or(NodeProcessStatus::NotStarted, status)),
        }
    }

    /// Only called once the previous child for `node` (if any) has exited.
    fn start(&mut self, node: &NodeConfig) -> io::Result<NodeProcessStatus> {
        let child = Command::new(&self.config.node_program)
            .args(node.to_args())
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()?;
        let pid = child.id();
        self.children.insert(node_ref(node), child);
        Ok(NodeProcessStatus::Running { pid })
    }

    fn terminate(&mut self, node: NodeRef) -> io::Result<NodeProcessStatus> {
        match self.children.get_mut(&node) {
            Some(child) => wait_or_kill(child, self.config.terminate_timeout),
            None => Ok(NodeProcessStatus::NotStarted),
        }
    }
}

fn node_ref(node: &NodeConfig) -> NodeRef {
    NodeRef {
        cluster: node.cluster,
        node_key: node.node_key,
    }
}

fn status(child: &mut Child) -> NodeProcessStatus {
    match child.try_wait() {
        Ok(None) => NodeProcessStatus::Running { pid: child.id() },
        Ok(Some(exit)) => NodeProcessStatus::Exited { code: exit.code() },
        Err(e) => NodeProcessStatus::Unknown {
            detail: e.to_string(),
        },
    }
}

/// Wait up to `timeout` for `child` to exit, then kill it. Returns the exit status.
fn wait_or_kill(child: &mut Child, timeout: Duration) -> io::Result<NodeProcessStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(exit) = child.try_wait()? {
            return Ok(NodeProcessStatus::Exited { code: exit.code() });
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let exit = child.wait()?;
            return Ok(NodeProcessStatus::Exited { code: exit.code() });
        }
        thread::sleep(EXIT_POLL);
    }
}
