//! Command ↔ Node control RR against a running [`NodeRuntime`] (#153).

use std::thread;
use std::time::{Duration, Instant};

use contract::test_support::{node_command_client, unique_cluster};
use contract::{
    HashedId, HostCtrl, IoxNode, MAX_NODE_PROBE_IDS, NodeCommand, NodeCommandClient,
    NodeCommandDto, NodeCommandError, NodeCommandResult, NodeConfig, NodeRequestError,
};
use sim_node::{NodeRuntime, SimState};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(5);
const NODE_KEY: HashedId<u64> = HashedId::new(0xdead_beef);

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
            node_key: NODE_KEY,
            board_count: 1,
        })
        .unwrap();
        let (iox, client) = node_command_client(dir.path(), cluster, NODE_KEY);
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

#[test]
fn shutdown_replies_then_stops_runtime() {
    let f = Fixture::new();
    assert_eq!(f.request(NodeCommand::Shutdown), NodeCommandResult::Success);
    let deadline = Instant::now() + TIMEOUT;
    while f.runtime.is_running() {
        assert!(Instant::now() < deadline, "runtime ignored Shutdown");
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        f.client
            .request(NodeCommand::Start, Duration::from_millis(50)),
        Err(NodeRequestError::NoServer)
    );
}
