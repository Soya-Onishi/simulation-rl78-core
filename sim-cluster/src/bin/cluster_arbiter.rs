//! `cluster-arbiter` — bind resources and coordinate node processes.

use std::env;
use std::path::PathBuf;
use std::process;

use sim_cluster::{ArbiterOptions, PeerAdvertise, run_arbiter};

fn main() {
    let mut topology: Option<PathBuf> = None;
    let mut node_bin: Option<PathBuf> = None;
    let mut plant_bin: Option<PathBuf> = None;
    let mut cluster_key: Option<String> = None;
    let mut iox_root: Option<PathBuf> = None;
    let mut local_boards: Option<Vec<String>> = None;
    let mut advertise_ip: Option<String> = None;
    let mut backend_key: Option<String> = None;
    let mut endpoints_out: Option<PathBuf> = None;
    let mut phase2_in: Option<PathBuf> = None;
    let mut time_sync_period_ms: Option<u64> = None;
    let mut assignment_in: Option<PathBuf> = None;
    let mut data_udp_window: Option<u32> = None;
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
            "--local-boards" => {
                let Some(list) = args.next() else {
                    eprintln!("cluster-arbiter: --local-boards requires a comma-separated list");
                    process::exit(2);
                };
                let ids: Vec<String> = list
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                local_boards = Some(ids);
            }
            "--advertise-ip" => {
                let Some(ip) = args.next() else {
                    eprintln!("cluster-arbiter: --advertise-ip requires an IPv4 address");
                    process::exit(2);
                };
                advertise_ip = Some(ip);
            }
            "--backend-key" => {
                let Some(key) = args.next() else {
                    eprintln!("cluster-arbiter: --backend-key requires ip:port");
                    process::exit(2);
                };
                backend_key = Some(key);
            }
            "--endpoints-out" => {
                let Some(path) = args.next() else {
                    eprintln!("cluster-arbiter: --endpoints-out requires a path");
                    process::exit(2);
                };
                endpoints_out = Some(PathBuf::from(path));
            }
            "--phase2-in" => {
                let Some(path) = args.next() else {
                    eprintln!("cluster-arbiter: --phase2-in requires a path");
                    process::exit(2);
                };
                phase2_in = Some(PathBuf::from(path));
            }
            "--time-sync-period-ms" => {
                let Some(ms) = args.next().and_then(|v| v.parse::<u64>().ok()) else {
                    eprintln!("cluster-arbiter: --time-sync-period-ms requires a positive integer");
                    process::exit(2);
                };
                if ms == 0 {
                    eprintln!("cluster-arbiter: --time-sync-period-ms must be greater than 0");
                    process::exit(2);
                }
                time_sync_period_ms = Some(ms);
            }
            "--assignment-in" => {
                let Some(path) = args.next() else {
                    eprintln!("cluster-arbiter: --assignment-in requires a path");
                    process::exit(2);
                };
                assignment_in = Some(PathBuf::from(path));
            }
            "--data-udp-window" => {
                let Some(n) = args.next().and_then(|v| v.parse::<u32>().ok()) else {
                    eprintln!("cluster-arbiter: --data-udp-window requires a positive integer");
                    process::exit(2);
                };
                if n == 0 {
                    eprintln!("cluster-arbiter: --data-udp-window must be greater than 0");
                    process::exit(2);
                }
                data_udp_window = Some(n);
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
    opts.cluster_key = cluster_key;
    opts.iox_root = iox_root;
    opts.local_board_ids = local_boards;
    opts.peer_advertise = match (advertise_ip, backend_key) {
        (None, None) => None,
        (Some(advertise_ip), Some(backend_key)) => Some(PeerAdvertise {
            advertise_ip,
            backend_key,
        }),
        _ => {
            eprintln!(
                "cluster-arbiter: --advertise-ip and --backend-key must both be set or both omitted"
            );
            process::exit(2);
        }
    };

    if endpoints_out.is_some() && opts.peer_advertise.is_none() {
        eprintln!("cluster-arbiter: --endpoints-out requires --advertise-ip and --backend-key");
        process::exit(2);
    }
    opts.endpoints_out = endpoints_out;
    if phase2_in.is_some() && opts.peer_advertise.is_none() {
        eprintln!("cluster-arbiter: --phase2-in requires --advertise-ip and --backend-key");
        process::exit(2);
    }
    opts.phase2_in = phase2_in;
    if let Some(ms) = time_sync_period_ms {
        opts.time_sync_period_ms = ms;
    }
    if assignment_in.is_some() && opts.peer_advertise.is_none() {
        eprintln!("cluster-arbiter: --assignment-in requires --advertise-ip and --backend-key");
        process::exit(2);
    }
    opts.assignment_in = assignment_in;
    if let Some(n) = data_udp_window {
        opts.data_udp_window = n;
    }

    if let Err(err) = run_arbiter(&opts) {
        eprintln!("cluster-arbiter: {err}");
        process::exit(1);
    }
}

fn usage() -> &'static str {
    "Usage: cluster-arbiter --topology <logical.json> --cluster-key <key> --iox-root <path> [--node-bin <path>] [--plant-bin <path>] [--local-boards id1,id2] [--advertise-ip <ipv4> --backend-key <ip:port> [--endpoints-out <path>] [--phase2-in <path>] [--time-sync-period-ms <ms>] [--assignment-in <path>] [--data-udp-window <n>]]\n\
     \n\
     Spawned by cluster-backend. Host commands arrive on iceoryx `ctrl/s2a`
     and status is published on `status/a2s`. Logs use `log/a2s`.\n\
     The process stays up after stop until a shutdown command.\n\
     Default board binary is rl78-minimal-board next to this executable.\n\
     Default plant binary is minimal-plant next to this executable.\n"
}
