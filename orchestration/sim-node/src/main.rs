//! `sim-node` process: runs [`NodeRuntime`] until Command sends
//! `NodeCommand::Shutdown`, the parent (Command) process exits, or a thread
//! fails, then shuts the threads down so iceoryx ports are released.

use std::os::unix::process::parent_id;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use contract::NodeConfig;
use sim_node::{DummyTcpStream, NodeRuntime, install_logger};

/// Main thread poll period for parent exit and thread exit.
const MAIN_POLL: Duration = Duration::from_millis(50);

fn main() -> ExitCode {
    // Must be the spawning Command: a parent that already exited is not detected.
    let parent = parent_id();
    let config = match NodeConfig::from_args(std::env::args_os().skip(1)) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("sim-node: {e}\n{}", NodeConfig::USAGE);
            return ExitCode::from(2);
        }
    };
    match run(config, parent) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sim-node: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(config: NodeConfig, parent: u32) -> Result<(), String> {
    install_logger();
    let runtime = NodeRuntime::spawn(config, DummyTcpStream).map_err(|e| e.to_string())?;
    while runtime.is_running() {
        if parent_id() != parent {
            eprintln!("sim-node: parent process {parent} exited; shutting down");
            break;
        }
        thread::sleep(MAIN_POLL);
    }
    runtime.shutdown().map_err(|e| e.to_string())
}
