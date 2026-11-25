//! `sim-node` binary exit paths: Shutdown command and parent exit (#153).

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use contract::test_support::{node_command_client, unique_cluster};
use contract::{ClusterId, HashedId, HostCtrl, NodeCommand, NodeCommandResult, NodeConfig};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(5);
/// First request also waits for the spawned binary to create its RR server.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const NODE_KEY: HashedId<u64> = HashedId::new(0xdead_beef);

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sim_node_command(dir: &TempDir, cluster: ClusterId) -> Command {
    let config = NodeConfig {
        iox_root: dir.path().to_path_buf(),
        cluster,
        node_key: NODE_KEY,
        board_count: 1,
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_sim-node"));
    command.args(config.to_args());
    command
}

/// Process `pid` has exited (gone, or a zombie nobody reaped yet).
fn process_exited(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z')),
    }
}

#[test]
fn node_binary_exits_on_shutdown_command() {
    let dir = tempfile::tempdir().unwrap();
    let _host_ctrl = HostCtrl::create(dir.path(), 1, false).unwrap();
    let cluster = unique_cluster();
    let mut child = KillOnDrop(
        sim_node_command(&dir, cluster)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (_iox, client) = node_command_client(dir.path(), cluster, NODE_KEY);
    assert_eq!(
        client.request(NodeCommand::Start, STARTUP_TIMEOUT),
        Ok(NodeCommandResult::Success)
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "sim-node exited early"
    );

    assert_eq!(
        client.request(NodeCommand::Shutdown, TIMEOUT),
        Ok(NodeCommandResult::Success)
    );
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "sim-node ignored Shutdown");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "sim-node exit status {status}");
    let mut stderr = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains("shutdown requested by Command"),
        "sim-node log missing from stderr: {stderr:?}"
    );
}

#[test]
fn node_binary_exits_when_parent_exits() {
    let dir = tempfile::tempdir().unwrap();
    let _host_ctrl = HostCtrl::create(dir.path(), 1, false).unwrap();
    let cluster = unique_cluster();
    let node = sim_node_command(&dir, cluster);
    // The shell stands in for Command: it starts sim-node, prints its pid,
    // and exits once its stdin is closed.
    let mut parent = KillOnDrop(
        Command::new("sh")
            .arg("-c")
            .arg(r#""$0" "$@" >/dev/null 2>&1 & echo $!; read _"#)
            .arg(node.get_program())
            .args(node.get_args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(parent.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let pid: u32 = line.trim().parse().unwrap();
    let (_iox, client) = node_command_client(dir.path(), cluster, NODE_KEY);
    assert_eq!(
        client.request(NodeCommand::Start, STARTUP_TIMEOUT),
        Ok(NodeCommandResult::Success)
    );

    drop(parent.0.stdin.take());
    parent.0.wait().unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while !process_exited(pid) {
        if Instant::now() >= deadline {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
            panic!("sim-node {pid} outlived its parent");
        }
        thread::sleep(Duration::from_millis(10));
    }
}
