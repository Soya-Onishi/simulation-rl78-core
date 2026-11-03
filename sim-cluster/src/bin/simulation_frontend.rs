//! `simulation-frontend` — browser HTTP, SSE, static UI, and GDB RSP.

use std::env;
use std::process;
use std::time::Duration;

use sim_cluster::backend::{TcpipBackend, check_health};
use sim_cluster::config::{ConfigCli, FrontendConfig, advertise_host_from_ip};
use sim_cluster::http;

fn main() {
    let path =
        match sim_cluster::config::parse_config_path(env::args().skip(1), "simulation-frontend") {
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

    let cfg = match FrontendConfig::load(&path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("simulation-frontend: {err}");
            process::exit(1);
        }
    };

    // TODO: Remove after supporting multiple server
    if cfg.servers.len() > 1 {
        eprintln!(
            "simulation-frontend: servers has {} entries; using the first only. \
             Dispatch to multiple backends is not implemented (TODO).",
            cfg.servers.len()
        );
    }

    let first = &cfg.servers[0];
    if let Err(err) = check_health(&first.ip, first.port, Duration::from_secs(2)) {
        eprintln!(
            "simulation-frontend: servers[0] {}:{} health check failed: {err}",
            first.ip, first.port
        );
        process::exit(1);
    }

    let backend = TcpipBackend::new(format!("http://{}:{}", first.ip, first.port));
    let listen = cfg.listen.socket_addr();
    let advertise = advertise_host_from_ip(&cfg.listen.ip);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("simulation-frontend: {err}");
            process::exit(1);
        }
    };
    if let Err(err) = rt.block_on(http::serve(&listen, advertise, backend)) {
        eprintln!("simulation-frontend: {err}");
        process::exit(1);
    }
}

fn usage() -> &'static str {
    "Usage: simulation-frontend --config <path>\n\
     \n\
     Serves per-simulation pages (/{id}). GET / is 404; create instances with\n\
     `python -m topology_dsl create <script.py>` (or scripts/create-sim).\n\
     Control API: POST /api/simulation, POST .../{id}/start|stop|reset,\n\
     DELETE .../{id}, GET .../{id} (status snapshot), GET .../{id}/status and .../{id}/log (SSE).\n\
     Start cluster-backend first. Relative --config paths are resolved from cwd.\n\
     Example: simulation-frontend --config example/config/frontend.yaml\n"
}
