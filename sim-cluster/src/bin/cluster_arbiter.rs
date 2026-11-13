//! `cluster-arbiter` — bind resources and coordinate node processes.

use std::env;
use std::path::PathBuf;
use std::process;

use sim_cluster::{ArbiterOptions, run_arbiter};

fn main() {
    let mut topology: Option<PathBuf> = None;
    let mut node_bin: Option<PathBuf> = None;
    let mut plant_bin: Option<PathBuf> = None;
    let mut piping_plant_bin: Option<PathBuf> = None;
    let mut boiler_plant_bin: Option<PathBuf> = None;
    let mut rl78boiler_bin: Option<PathBuf> = None;
    let mut cluster_key: Option<String> = None;
    let mut iox_root: Option<PathBuf> = None;
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
            "--plant-bin" => {
                let Some(path) = args.next() else {
                    eprintln!("cluster-arbiter: --plant-bin requires a path");
                    process::exit(2);
                };
                plant_bin = Some(PathBuf::from(path));
            }
            "--piping-plant-bin" => {
                let Some(path) = args.next() else {
                    eprintln!("cluster-arbiter: --piping-plant-bin requires a path");
                    process::exit(2);
                };
                piping_plant_bin = Some(PathBuf::from(path));
            }
            "--boiler-plant-bin" => {
                let Some(path) = args.next() else {
                    eprintln!("cluster-arbiter: --boiler-plant-bin requires a path");
                    process::exit(2);
                };
                boiler_plant_bin = Some(PathBuf::from(path));
            }
            "--rl78boiler-bin" => {
                let Some(path) = args.next() else {
                    eprintln!("cluster-arbiter: --rl78boiler-bin requires a path");
                    process::exit(2);
                };
                rl78boiler_bin = Some(PathBuf::from(path));
            }
            "--cluster-key" => {
                let Some(key) = args.next() else {
                    eprintln!("cluster-arbiter: --cluster-key requires a value");
                    process::exit(2);
                };
                cluster_key = Some(key);
            }
            "--iox-root" => {
                let Some(path) = args.next() else {
                    eprintln!("cluster-arbiter: --iox-root requires a path");
                    process::exit(2);
                };
                iox_root = Some(PathBuf::from(path));
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
    let (Some(cluster_key), Some(iox_root)) = (cluster_key, iox_root) else {
        eprintln!("cluster-arbiter: --cluster-key and --iox-root are required");
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
    if let Some(plant_bin) = plant_bin {
        opts.plant_bin = plant_bin;
    }
    if let Some(piping_plant_bin) = piping_plant_bin {
        opts.piping_plant_bin = piping_plant_bin;
    }
    if let Some(boiler_plant_bin) = boiler_plant_bin {
        opts.boiler_plant_bin = boiler_plant_bin;
    }
    if let Some(rl78boiler_bin) = rl78boiler_bin {
        opts.rl78boiler_bin = rl78boiler_bin;
    }
    opts.cluster_key = cluster_key;
    opts.iox_root = iox_root;

    if let Err(err) = run_arbiter(&opts) {
        eprintln!("cluster-arbiter: {err}");
        process::exit(1);
    }
}

fn usage() -> &'static str {
    "Usage: cluster-arbiter --topology <logical.json> --cluster-key <key> --iox-root <path>\n\
     \t[--node-bin <path>] [--plant-bin <path>]\n\
     \t[--piping-plant-bin <path>] [--boiler-plant-bin <path>] [--rl78boiler-bin <path>]\n\
     \n\
     Spawned by cluster-backend. Host commands arrive on iceoryx `ctrl/s2a`
     and status is published on `status/a2s`. Logs use `log/a2s`.\n\
     The process stays up after stop until a shutdown command.\n\
     Kind → default binary (next to this executable):\n\
       rl78          → rl78-minimal-board (--node-bin)\n\
       plant         → minimal-plant (--plant-bin)\n\
       piping-plant  → piping-plant (--piping-plant-bin)\n\
       boiler-plant  → boiler-plant (--boiler-plant-bin)\n\
       rl78boiler    → boiler_board (--rl78boiler-bin)\n"
}
