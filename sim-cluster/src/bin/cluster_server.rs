//! `cluster-server` — simulation control and the status page, in one process.

use std::env;
use std::process;

use sim_cluster::{ServerOptions, run_server};

fn main() {
    let mut listen = "127.0.0.1:8090".to_string();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                eprint!("{}", usage());
                process::exit(0);
            }
            "--listen" => {
                let Some(addr) = args.next() else {
                    eprintln!("cluster-server: --listen requires host:port");
                    eprint!("{}", usage());
                    process::exit(2);
                };
                listen = addr;
            }
            other => {
                eprintln!("cluster-server: unknown argument {other}");
                eprint!("{}", usage());
                process::exit(2);
            }
        }
    }

    let opts = match ServerOptions::new(listen) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("cluster-server: {err}");
            process::exit(1);
        }
    };
    if let Err(err) = run_server(&opts) {
        eprintln!("cluster-server: {err}");
        process::exit(1);
    }
}

fn usage() -> &'static str {
    "Usage: cluster-server [--listen host:port]\n\
     \n\
     Serves per-simulation pages (/{id}). GET / is 404; create instances with\n\
     `python -m topology_dsl create <script.py>` (or scripts/create-sim).\n\
     Control API: POST /api/simulation, POST .../{id}/start|stop|reset,\n\
     DELETE .../{id}, GET .../{id} (status snapshot), GET .../{id}/status and .../{id}/log (SSE).\n\
     Default listen address is 127.0.0.1:8090.\n\
     The GDB ssh command uses this host (0.0.0.0 becomes 127.0.0.1).\n\
     \n\
     HTTP create accepts topology JSON only. Each board elf must be http(s)://\n\
     or base64: (embed local files on the client). Logs stream over SSE.\n"
}
