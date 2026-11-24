//! Command ↔ Node control RR against a running [`NodeRuntime`] (#153).

use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use contract::{
    ClusterId, HashedId, HostCtrl, IoxNode, MAX_NODE_PROBE_IDS, NodeCommand, NodeCommandClient,
    NodeCommandDto, NodeCommandError, NodeCommandResult, create_iox_node, isolated_config,
};
use sim_node::{NodeConfig, NodeRuntime, SimState};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Service shared memory is keyed by name only; keep concurrent tests apart.
fn unique_cluster() -> ClusterId {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    ClusterId::new(std::process::id() ^ (seq << 24), nanos)
}

fn node_key() -> HashedId<u64> {
    HashedId::new(0xdead_beef)
}

struct Fixture {
    client: NodeCommandClient,
    _iox: IoxNode,
    runtime: NodeRuntime,
    host_ctrl: HostCtrl,
    _dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let host_ctrl = HostCtrl::create(dir.path(), 1, false).unwrap();
        let cluster = unique_cluster();
        let runtime = NodeRuntime::spawn(NodeConfig {
            iox_root: dir.path().to_path_buf(),
            cluster,
            node_key: node_key(),
            board_count: 1,
        })
        .unwrap();
        let (iox, client) = client(&dir, cluster);
        Self {
            client,
            _iox: iox,
            runtime,
            host_ctrl,
            _dir: dir,
        }
    }

    fn request(&self, command: NodeCommand) -> NodeCommandResult {
        self.client.request(command, TIMEOUT).unwrap()
    }

    fn wait_for_state(&self, want: SimState) {
        let deadline = Instant::now() + TIMEOUT;
        while self.runtime.state() != want {
            assert!(Instant::now() < deadline, "state never became {want:?}");
            thread::sleep(Duration::from_millis(1));
        }
    }
}

fn client(dir: &TempDir, cluster: ClusterId) -> (IoxNode, NodeCommandClient) {
    let cfg = isolated_config(dir.path()).unwrap();
    let iox = create_iox_node(&cfg, "test-command").unwrap();
    let client = NodeCommandClient::create(&iox, cluster, node_key()).unwrap();
    (iox, client)
}

fn subscribe(ids: std::ops::Range<u64>) -> NodeCommand {
    let ids: Vec<_> = ids.map(HashedId::new).collect();
    NodeCommand::probe_subscribe(&ids).unwrap()
}

#[test]
fn start_stop_reset_follow_state_table() {
    let f = Fixture::new();
    assert_eq!(f.runtime.state(), SimState::Stopped);

    assert_eq!(f.request(NodeCommand::Start), NodeCommandResult::Success);
    assert_eq!(f.runtime.state(), SimState::Running);
    assert_eq!(f.request(NodeCommand::Reset), NodeCommandResult::Success);
    assert_eq!(f.runtime.state(), SimState::Stopped);
    assert_eq!(f.request(NodeCommand::Stop), NodeCommandResult::Success);
    assert_eq!(f.runtime.state(), SimState::Stopped);
}

#[test]
fn stop_command_stops_running_simulation() {
    let f = Fixture::new();
    assert_eq!(f.request(NodeCommand::Start), NodeCommandResult::Success);
    assert_eq!(f.request(NodeCommand::Stop), NodeCommandResult::Success);
    assert_eq!(f.runtime.state(), SimState::Stopped);
    assert_eq!(f.host_ctrl.stop_generation(), 0);
}

#[test]
fn bump_stop_emergency_stops_running_simulation() {
    let f = Fixture::new();
    assert_eq!(f.request(NodeCommand::Start), NodeCommandResult::Success);
    f.host_ctrl.bump_stop();
    f.wait_for_state(SimState::Stopped);

    assert_eq!(f.request(NodeCommand::Start), NodeCommandResult::Success);
    thread::sleep(Duration::from_millis(20));
    assert_eq!(f.runtime.state(), SimState::Running);
}

#[test]
fn start_right_after_bump_stop_keeps_running() {
    let f = Fixture::new();
    assert_eq!(f.request(NodeCommand::Start), NodeCommandResult::Success);
    f.host_ctrl.bump_stop();
    assert_eq!(f.request(NodeCommand::Start), NodeCommandResult::Success);
    thread::sleep(Duration::from_millis(20));
    assert_eq!(f.runtime.state(), SimState::Running);
}

#[test]
fn unimplemented_commands_are_unavailable() {
    let f = Fixture::new();
    let unavailable = NodeCommandResult::Error(NodeCommandError::Unavailable);
    assert_eq!(f.request(NodeCommand::GdbStep), unavailable);
    assert_eq!(
        f.request(NodeCommand::GdbStepSync { until_ns: 100 }),
        unavailable
    );
    assert_eq!(
        f.request(NodeCommand::inspect(&[1, 2]).unwrap()),
        unavailable
    );
    let action = NodeCommandDto::Action {
        device: HashedId::new(1),
        action: HashedId::new(2),
        params: vec![0.5],
    }
    .to_wire()
    .unwrap();
    assert_eq!(f.request(action), unavailable);
}

#[test]
fn probe_subscribe_merges_and_rejects_overflow() {
    let f = Fixture::new();
    let bad = NodeCommandResult::Error(NodeCommandError::BadParam);
    assert_eq!(f.request(subscribe(0..20)), NodeCommandResult::Success);
    assert_eq!(f.request(subscribe(10..30)), NodeCommandResult::Success);
    assert_eq!(f.request(subscribe(30..33)), bad);
    assert_eq!(f.request(subscribe(30..32)), NodeCommandResult::Success);
    assert_eq!(f.request(subscribe(0..32)), NodeCommandResult::Success);
    assert_eq!(f.request(subscribe(32..33)), bad);
}

#[test]
fn malformed_counts_are_bad_param() {
    let f = Fixture::new();
    let command = NodeCommand::ProbeSubscribe {
        count: MAX_NODE_PROBE_IDS as u8 + 1,
        probe_ids: [HashedId::ZERO; MAX_NODE_PROBE_IDS],
    };
    assert_eq!(
        f.request(command),
        NodeCommandResult::Error(NodeCommandError::BadParam)
    );
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn node_binary_serves_rr_in_separate_process() {
    let dir = tempfile::tempdir().unwrap();
    let _host_ctrl = HostCtrl::create(dir.path(), 1, false).unwrap();
    let cluster = unique_cluster();
    let mut child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_sim-node"))
            .arg("--iox-root")
            .arg(dir.path())
            .args(["--cluster", &cluster.to_string()])
            .args(["--node-key", &node_key().to_string()])
            .args(["--board-count", "1"])
            .spawn()
            .unwrap(),
    );
    let (_iox, client) = client(&dir, cluster);
    assert_eq!(
        client.request(NodeCommand::Start, Duration::from_secs(10)),
        Ok(NodeCommandResult::Success)
    );
    assert_eq!(
        client.request(NodeCommand::Stop, TIMEOUT),
        Ok(NodeCommandResult::Success)
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "sim-node exited early"
    );

    let pid = child.0.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "sim-node ignored SIGTERM");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "sim-node exit status {status}");
}
