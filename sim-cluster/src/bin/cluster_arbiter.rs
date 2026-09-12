//! `cluster-arbiter` — bind resources and coordinate node processes.

use std::env;
use std::path::PathBuf;
use std::process;

use sim_cluster::{ArbiterOptions, run_arbiter};

fn main() {
    let mut topology: Option<PathBuf> = None;
    let mut node_bin: Option<PathBuf> = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                eprint!("{}", usage());
                process::exit(0);
            }
            "--topology" => {
                let Some(path) = args.next() else {
                    eprintln!("cluster-arbiter: --topology requires a path");
                    process::exit(2);
                };
                topology = Some(PathBuf::from(path));
            }
            "--node-bin" => {
                let Some(path) = args.next() else {
                    eprintln!("cluster-arbiter: --node-bin requires a path");
                    process::exit(2);
                };
                node_bin = Some(PathBuf::from(path));
            }
            other => {
                eprintln!("cluster-arbiter: unknown argument {other}");
                eprint!("{}", usage());
                process::exit(2);
            }
        }
    }
    let Some(topology) = topology else {
        eprintln!("cluster-arbiter: --topology is required");
        eprint!("{}", usage());
        process::exit(2);
    };

    let mut opts = match ArbiterOptions::from_topology_path(topology) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("cluster-arbiter: {err}");
            process::exit(1);
        }
    };
    if let Some(node_bin) = node_bin {
        opts.node_bin = node_bin;
    }

    if let Err(err) = run_arbiter(&opts) {
        eprintln!("cluster-arbiter: {err}");
        process::exit(1);
    }
}

fn usage() -> &'static str {
    "Usage: cluster-arbiter --topology <logical.json> [--node-bin <path>]\n\
     \n\
     Normally spawned by cluster-server with a validated logical topology.\n"
}
