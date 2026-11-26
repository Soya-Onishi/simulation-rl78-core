//! REST → Command → Node RR → state transitions with real `sim-node`
//! processes launched by the Command host's Build thread (#147 H).

use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::StatusCode;
use contract::test_support::{node_command_client, unique_cluster};
use contract::{
    ClusterId, HashedId, HostCtrl, NodeCommand, NodeCommandResult, NodeConfig, build_ensure,
    build_restart, build_status, build_terminate, node_command, node_key, probe_interest, reset,
    start, status, stop,
};
use serde_json::{Value, json};
use sim_command::test_support::{call, create_session};
use sim_command::{CommandConfig, CommandHost};
use tempfile::TempDir;

const A: &str = "board-a";
const B: &str = "board-b";
const TIMEOUT: Duration = Duration::from_secs(5);

struct Host {
    host: CommandHost,
    router: Router,
    // Last: dropping `host` still cleans up session directories inside it.
    dir: TempDir,
}

impl Host {
    fn config(dir: &Path) -> CommandConfig {
        CommandConfig::new(dir.join("work"), env!("CARGO_BIN_EXE_sim-node").into())
    }

    fn spawn() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let host = CommandHost::spawn(Self::config(dir.path())).unwrap();
        let router = host.router();
        Self { dir, host, router }
    }

    fn session_root(&self, cluster: ClusterId) -> std::path::PathBuf {
        Self::config(self.dir.path()).session_root(cluster)
    }

    async fn post(&self, uri: &str, body: Option<Value>) -> Value {
        let (code, reply) = call(&self.router, "POST", uri, body).await;
        assert_eq!(code, StatusCode::OK, "POST {uri}: {reply}");
        reply
    }

    async fn ensure(&self, cluster: ClusterId, board: &str) -> u32 {
        assert_eq!(
            self.post(&build_ensure::url(cluster, node_key(board)), None)
                .await,
            json!({"ok": "ack"})
        );
        let status = self.build_status(cluster, board).await;
        assert_eq!(status["lifecycle"], "running", "{status}");
        u32::try_from(status["pid"].as_u64().unwrap()).unwrap()
    }

    async fn build_status(&self, cluster: ClusterId, board: &str) -> Value {
        let uri = build_status::url(cluster, node_key(board));
        let (code, reply) = call(&self.router, "GET", &uri, None).await;
        assert_eq!(code, StatusCode::OK, "{reply}");
        reply["status"].clone()
    }

    async fn state(&self, cluster: ClusterId) -> Value {
        let (code, reply) = call(&self.router, "GET", &status::url(cluster), None).await;
        assert_eq!(code, StatusCode::OK, "{reply}");
        reply["state"].clone()
    }

    async fn node_command(&self, cluster: ClusterId, board: &str, command: Value) -> Value {
        let uri = node_command::url(cluster, node_key(board));
        self.post(&uri, Some(json!({"command": command}))).await
    }
}

fn process_gone(pid: u32) -> bool {
    !Path::new(&format!("/proc/{pid}")).exists()
}

#[tokio::test]
async fn session_commands_drive_every_node() {
    let h = Host::spawn();
    let cluster = create_session(&h.router, &[A, B], &[A, B]).await;
    let pids = [h.ensure(cluster, A).await, h.ensure(cluster, B).await];

    h.post(&start::url(cluster), Some(json!({}))).await;
    assert_eq!(h.state(cluster).await, "running");
    h.post(&stop::url(cluster), None).await;
    assert_eq!(h.state(cluster).await, "idle");
    h.post(&start::url(cluster), Some(json!({}))).await;
    assert_eq!(h.state(cluster).await, "running");
    h.post(&reset::url(cluster), None).await;
    assert_eq!(h.state(cluster).await, "idle");

    assert_eq!(
        h.node_command(cluster, A, json!({"type": "stop"})).await,
        json!({"ok": "node", "type": "success"})
    );
    assert_eq!(
        h.node_command(cluster, B, json!({"type": "gdb_step"}))
            .await,
        json!({"ok": "node", "type": "error", "code": "unavailable"})
    );
    let probe = probe_interest::url(cluster, node_key(A));
    assert_eq!(
        h.post(&probe, Some(json!({"probe_ids": [HashedId::new(7u64)]})))
            .await,
        json!({"ok": "node", "type": "success"})
    );

    h.host.shutdown().unwrap();
    for pid in pids {
        assert!(
            process_gone(pid),
            "sim-node {pid} survived Command shutdown"
        );
    }
}

#[tokio::test]
async fn emergency_stop_makes_status_not_running() {
    let h = Host::spawn();
    let cluster = create_session(&h.router, &[A, B], &[A]).await;
    h.ensure(cluster, A).await;
    h.post(&start::url(cluster), Some(json!({}))).await;
    assert_eq!(h.state(cluster).await, "running");

    HostCtrl::open(&h.session_root(cluster), 2)
        .unwrap()
        .bump_stop();
    assert_eq!(h.state(cluster).await, "idle");

    h.post(&start::url(cluster), Some(json!({}))).await;
    assert_eq!(h.state(cluster).await, "running");
}

#[tokio::test]
async fn terminate_and_restart_act_on_the_node_process() {
    let h = Host::spawn();
    let cluster = create_session(&h.router, &[A], &[A]).await;
    let first = h.ensure(cluster, A).await;

    h.post(&build_restart::url(cluster, node_key(A)), None)
        .await;
    let status = h.build_status(cluster, A).await;
    let second = u32::try_from(status["pid"].as_u64().unwrap()).unwrap();
    assert_ne!(second, first);
    assert!(process_gone(first));
    h.post(&start::url(cluster), Some(json!({}))).await;

    h.post(&build_terminate::url(cluster, node_key(A)), None)
        .await;
    assert_eq!(
        h.build_status(cluster, A).await,
        json!({"lifecycle": "exited", "code": 0}),
        "sim-node must exit cleanly on Shutdown"
    );
    let (code, _) = call(
        &h.router,
        "POST",
        &node_command::url(cluster, node_key(A)),
        Some(json!({"command": {"type": "start"}})),
    )
    .await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "terminated node must leave the registration table"
    );
}

#[tokio::test]
async fn restart_recovers_a_killed_node() {
    let h = Host::spawn();
    let cluster = create_session(&h.router, &[A], &[A]).await;
    let pid = h.ensure(cluster, A).await;
    let killed = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + TIMEOUT;
    while h.build_status(cluster, A).await["lifecycle"] == "running" {
        assert!(
            Instant::now() < deadline,
            "killed sim-node {pid} still running"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    h.post(&build_restart::url(cluster, node_key(A)), None)
        .await;
    h.post(&start::url(cluster), Some(json!({}))).await;
    assert_eq!(h.state(cluster).await, "running");
    h.host.shutdown().unwrap();
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A `sim-node` started outside any Command host, answering RR under `work_dir`.
struct OrphanNode {
    child: KillOnDrop,
    config: NodeConfig,
}

impl OrphanNode {
    fn spawn(work_dir: &Path) -> Self {
        let cluster = unique_cluster();
        let config = NodeConfig {
            iox_root: work_dir.join(cluster.to_string()),
            cluster,
            node_key: node_key(A),
            board_count: 1,
        };
        let _host_ctrl = HostCtrl::create(&config.iox_root, 1, false).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_sim-node"))
            .args(config.to_args())
            .spawn()
            .unwrap();
        let node = Self {
            child: KillOnDrop(child),
            config,
        };
        assert_eq!(
            node.request(NodeCommand::Start),
            Ok(NodeCommandResult::Success)
        );
        node
    }

    fn request(
        &self,
        command: NodeCommand,
    ) -> Result<NodeCommandResult, contract::NodeRequestError> {
        let (_iox, client) = node_command_client(
            &self.config.iox_root,
            self.config.cluster,
            self.config.node_key,
        );
        client.request(command, TIMEOUT)
    }

    /// Exit through `Shutdown` so the Node releases its shared memory.
    fn shut_down(mut self) {
        assert_eq!(
            self.request(NodeCommand::Shutdown),
            Ok(NodeCommandResult::Success)
        );
        assert!(self.child.0.wait().unwrap().success());
    }
}

#[test]
fn spawn_removes_sessions_of_killed_nodes_but_keeps_live_ones() {
    let dir = tempfile::tempdir().unwrap();
    let config = Host::config(dir.path());
    let mut killed = OrphanNode::spawn(&config.work_dir);
    let alive = OrphanNode::spawn(&config.work_dir);
    killed.child.0.kill().unwrap();
    killed.child.0.wait().unwrap();

    let host = CommandHost::spawn(config).unwrap();
    assert!(
        !killed.config.iox_root.exists(),
        "dead node session was not removed"
    );
    assert!(
        alive.config.iox_root.is_dir(),
        "live node session was removed"
    );
    host.shutdown().unwrap();
    alive.shut_down();
}
