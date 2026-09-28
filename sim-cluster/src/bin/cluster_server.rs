//! `cluster-server` — simulation control and the status page, in one process.

use std::env;
use std::path::PathBuf;
use std::process;

use sim_cluster::{ServerOptions, run_server};

fn main() {
    let mut listen = "127.0.0.1:8090".to_string();
    let mut topology: Option<PathBuf> = None;
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
            "--topology" => {
                let Some(path) = args.next() else {
                    eprintln!("cluster-server: --topology requires a .py or .json file");
                    eprint!("{}", usage());
                    process::exit(2);
                };
                topology = Some(PathBuf::from(path));
            }
            other => {
                eprintln!("cluster-server: unknown argument {other}");
                eprint!("{}", usage());
                process::exit(2);
            }
        }
    }

    let mut opts = match ServerOptions::new(listen) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("cluster-server: {err}");
            process::exit(1);
        }
    };
    opts.topology = topology;
    if let Err(err) = run_server(&opts) {
        eprintln!("cluster-server: {err}");
        process::exit(1);
    }
}

fn usage() -> &'static str {
    "Usage: cluster-server [--listen host:port] [--topology file.py|file.json]\n\
     \n\
     Serves the simulation page and runs the arbiter in this process.\n\
     With --topology, that simulation is loaded stopped until Start.\n\
     Default listen address is 127.0.0.1:8090. Open the printed URL.\n\
     \n\
     Local elf paths are read here and stored as base64:. http(s) and base64:\n\
     are kept as written. Logs are kept in this process and filtered only\n\
     in the page.\n"
}
