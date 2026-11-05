//! `simulation-frontend` — browser HTTP, SSE, static UI, and GDB RSP.

use std::env;
use std::process;
use std::sync::Arc;
use std::time::Duration;

use sim_cluster::backend::{HealthBody, SimulationBackend, TcpipBackend, fetch_health};
use sim_cluster::config::{ConfigCli, FrontendConfig, advertise_host_from_ip};
use sim_cluster::http;
use sim_cluster::orchestrate::MultiBackend;

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

    let timeout = Duration::from_secs(2);
    let mut results: Vec<Result<HealthBody, String>> = Vec::with_capacity(cfg.servers.len());
    for (index, server) in cfg.servers.iter().enumerate() {
        let result = fetch_health(&server.ip, server.port, timeout);
        match &result {
            Ok(body) => {
                eprintln!(
                    "simulation-frontend: servers[{index}] {}:{} health ok \
                     (max_boards={}, running_boards={})",
                    server.ip, server.port, body.max_boards, body.running_boards
                );
            }
            Err(err) => {
                eprintln!(
                    "simulation-frontend: servers[{index}] {}:{} health failed: {err}",
                    server.ip, server.port
                );
            }
        }
        results.push(result);
    }

    // One server: talk to it directly (legacy path). Several: MultiBackend probes
    // health on every create and runs the two-phase create over healthy servers.
    let backend: Arc<dyn SimulationBackend> = if cfg.servers.len() == 1 {
        if let Err(err) = &results[0] {
            eprintln!(
                "simulation-frontend: servers[0] {}:{} must be healthy: {err}",
                cfg.servers[0].ip, cfg.servers[0].port
            );
            process::exit(1);
        }
        let first = &cfg.servers[0];
        TcpipBackend::new(format!("http://{}:{}", first.ip, first.port))
    } else {
        if results.iter().all(Result::is_err) {
            eprintln!("simulation-frontend: no server is healthy");
            process::exit(1);
        }
        MultiBackend::new(cfg.servers.clone())
    };
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
    if let Err(err) = rt.block_on(http::serve(
        &listen,
        advertise,
        backend,
        cfg.web_root.clone(),
    )) {
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
