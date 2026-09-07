//! `cluster-node` — one board process (Phase 2: Ready/Start handshake only).

use std::env;
use std::path::PathBuf;
use std::process;

use sim_cluster::run_node;

fn main() {
    let mut board_id: Option<String> = None;
    let mut control: Option<PathBuf> = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                eprint!("{}", usage());
                process::exit(0);
            }
            "--board-id" => {
                board_id = Some(args.next().unwrap_or_else(|| {
                    eprintln!("cluster-node: --board-id requires a value");
                    process::exit(2);
                }));
            }
            "--control" => {
                control = Some(PathBuf::from(args.next().unwrap_or_else(|| {
                    eprintln!("cluster-node: --control requires a path");
                    process::exit(2);
                })));
            }
            other => {
                eprintln!("cluster-node: unknown argument {other}");
                eprint!("{}", usage());
                process::exit(2);
            }
        }
    }
    let (Some(board_id), Some(control)) = (board_id, control) else {
        eprintln!("cluster-node: --board-id and --control are required");
        eprint!("{}", usage());
        process::exit(2);
    };

    if let Err(err) = run_node(&board_id, &control) {
        eprintln!("cluster-node: {err}");
        process::exit(1);
    }
}

fn usage() -> &'static str {
    "Usage: cluster-node --board-id <id> --control <uds-path>\n\
     \n\
     Spawned by cluster-arbiter. Completes Ready/Start on the control plane.\n"
}
