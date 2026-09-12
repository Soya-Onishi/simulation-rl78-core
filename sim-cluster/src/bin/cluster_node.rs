//! `cluster-node` — one board process (control plane + optional HostStop inject).

use std::env;
use std::path::PathBuf;
use std::process;

use sim_cluster::{InjectHostStop, NodeOptions, run_node};

fn main() {
    let mut board_id: Option<String> = None;
    let mut control: Option<PathBuf> = None;
    let mut inject_reason: Option<String> = None;
    let mut inject_after_ms: u64 = 20;
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
            "--inject-host-stop" => {
                inject_reason = Some(args.next().unwrap_or_else(|| {
                    eprintln!("cluster-node: --inject-host-stop requires a reason");
                    process::exit(2);
                }));
            }
            "--inject-after-ms" => {
                let raw = args.next().unwrap_or_else(|| {
                    eprintln!("cluster-node: --inject-after-ms requires a value");
                    process::exit(2);
                });
                inject_after_ms = raw.parse().unwrap_or_else(|_| {
                    eprintln!("cluster-node: invalid --inject-after-ms {raw}");
                    process::exit(2);
                });
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

    let inject_host_stop = inject_reason.map(|reason| InjectHostStop {
        after_ms: inject_after_ms,
        reason,
    });

    if let Err(err) = run_node(NodeOptions {
        board_id,
        control,
        inject_host_stop,
    }) {
        eprintln!("cluster-node: {err}");
        process::exit(1);
    }
}

fn usage() -> &'static str {
    "Usage: cluster-node --board-id <id> --control <uds-path>\n\
     \n\
     Optional:\n\
       --inject-host-stop <reason>   Send HostStop after Start (smoke / E2E)\n\
       --inject-after-ms <ms>        Delay before inject (default 20)\n\
     \n\
     Spawned by cluster-arbiter. Completes Ready/Start on the control plane.\n"
}
