//! `cluster-backend` — simulation worker and internal HTTP API.

use std::env;
use std::process;

use sim_cluster::backend::BackendInfo;
use sim_cluster::config::{BackendConfig, ConfigCli};
use sim_cluster::{ServerOptions, run_backend};

fn main() {
    let path = match sim_cluster::config::parse_config_path(env::args().skip(1), "cluster-backend")
    {
        Ok(path) => path,
        Err(ConfigCli::Help) => {
            eprint!("{}", usage());
            process::exit(0);
        }
        Err(ConfigCli::Failed { message, exit_code }) => {
            eprintln!("{message}");
            eprint!("{}", usage());
            process::exit(exit_code);
        }
    };

    let cfg = match BackendConfig::load(&path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("cluster-backend: {err}");
            process::exit(1);
        }
    };

    let mut opts = match ServerOptions::from_current_exe(cfg.advertise_ip.clone()) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("cluster-backend: {err}");
            process::exit(1);
        }
    };
    opts.time_sync_period_ms = cfg.time_sync_period_ms;
    opts.data_udp_window = cfg.data_udp_window;
    let info = BackendInfo {
        max_boards: cfg.max_boards,
        time_sync_period_ms: cfg.time_sync_period_ms,
        data_udp_window: cfg.data_udp_window,
        multicast_base: cfg.multicast_base.clone(),
    };
    if let Err(err) = run_backend(&cfg.listen.socket_addr(), opts, info) {
        eprintln!("cluster-backend: {err}");
        process::exit(1);
    }
}

fn usage() -> &'static str {
    "Usage: cluster-backend --config <path>\n\
     \n\
     Internal SimulationBackend HTTP under /internal. Start this process before\n\
     simulation-frontend. Relative --config paths are resolved from cwd.\n\
     Example: cluster-backend --config example/config/backend.yaml\n"
}
