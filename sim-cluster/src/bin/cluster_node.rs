//! `cluster-node` — one board process (control plane + optional HostStop inject).

use std::env;
use std::path::PathBuf;
use std::process;

use sim_cluster::{InjectHostStop, NodeOptions, run_node};

fn main() {
    let mut board_id: Option<String> = None;
    let mut cluster_key: Option<String> = None;
    let mut iox_root: Option<PathBuf> = None;
    let mut topology: Option<PathBuf> = None;
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
            "--cluster-key" => {
                cluster_key = Some(args.next().unwrap_or_else(|| {
                    eprintln!("cluster-node: --cluster-key requires a value");
                    process::exit(2);
                }));
            }
            "--iox-root" => {
                iox_root = Some(PathBuf::from(args.next().unwrap_or_else(|| {
                    eprintln!("cluster-node: --iox-root requires a path");
                    process::exit(2);
                })));
            }
            "--topology" => {
                topology = Some(PathBuf::from(args.next().unwrap_or_else(|| {
                    eprintln!("cluster-node: --topology requires a path");
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
    let (Some(board_id), Some(cluster_key), Some(iox_root), Some(topology_path)) =
        (board_id, cluster_key, iox_root, topology)
    else {
        eprintln!(
            "cluster-node: --board-id, --cluster-key, --iox-root, and --topology are required"
        );
        eprint!("{}", usage());
        process::exit(2);
    };

    let inject_host_stop = inject_reason.map(|reason| InjectHostStop {
        after_ms: inject_after_ms,
        reason,
    });

    if let Err(err) = run_node(NodeOptions {
        board_id,
        cluster_key,
        iox_root,
        topology_path,
        inject_host_stop,
    }) {
        eprintln!("cluster-node: {err}");
        process::exit(1);
    }
}

fn usage() -> &'static str {
    "Usage: cluster-node --board-id <id> --cluster-key <key> --iox-root <path> --topology <json>\n\
     \n\
     Optional:\n\
       --inject-host-stop <reason>   Send HostStop after Start (smoke / E2E)\n\
       --inject-after-ms <ms>        Delay before inject (default 20)\n\
     \n\
     Spawned by cluster-arbiter. Completes Ready/Start on the control plane.\n"
}
