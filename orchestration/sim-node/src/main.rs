//! `sim-node` process: runs [`NodeRuntime`] until SIGINT / SIGTERM or a fatal
//! thread error, then shuts the threads down so iceoryx ports are released.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use contract::{create_signal_iox_node, isolated_config};
use sim_node::{NodeConfig, NodeRuntime};

const USAGE: &str =
    "usage: sim-node --iox-root <dir> --cluster <id-nano> --node-key <hex16> --board-count <n>";

/// Main thread poll period for termination signals and thread exit.
const MAIN_POLL: Duration = Duration::from_millis(50);

fn main() -> ExitCode {
    let config = match parse_args(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("sim-node: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sim-node: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(config: NodeConfig) -> Result<(), String> {
    // Before spawning, so SIGTERM is already captured once the RR server is up.
    let signals = isolated_config(&config.iox_root)
        .and_then(|cfg| create_signal_iox_node(&cfg, &format!("sim-node-{}-main", config.node_key)))
        .map_err(|e| e.to_string())?;
    let runtime = NodeRuntime::spawn(config).map_err(|e| e.to_string())?;
    while runtime.is_running() && signals.wait(MAIN_POLL).is_ok() {}
    runtime.shutdown().map_err(|e| e.to_string())
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<NodeConfig, String> {
    let mut iox_root = None;
    let mut cluster = None;
    let mut node_key = None;
    let mut board_count = None;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--iox-root" => iox_root = Some(PathBuf::from(value)),
            "--cluster" => {
                cluster = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid --cluster {value}"))?,
                );
            }
            "--node-key" => {
                node_key = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid --node-key {value}"))?,
                );
            }
            "--board-count" => {
                board_count = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid --board-count {value}"))?,
                );
            }
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    Ok(NodeConfig {
        iox_root: iox_root.ok_or("missing --iox-root")?,
        cluster: cluster.ok_or("missing --cluster")?,
        node_key: node_key.ok_or("missing --node-key")?,
        board_count: board_count.ok_or("missing --board-count")?,
    })
}
