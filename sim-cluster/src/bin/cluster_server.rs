//! `cluster-server` — singleton entry: run Python DSL, spawn `cluster-arbiter`.

use std::env;
use std::path::PathBuf;
use std::process;

use sim_cluster::{ServerError, ServerOptions, run_server};

fn main() {
    let mut args = env::args().skip(1);
    let script = match args.next() {
        Some(s) if s == "-h" || s == "--help" => {
            eprint!("{}", usage());
            process::exit(0);
        }
        Some(s) => PathBuf::from(s),
        None => {
            eprint!("{}", usage());
            process::exit(2);
        }
    };
    if args.next().is_some() {
        eprintln!("cluster-server: unexpected extra arguments");
        eprint!("{}", usage());
        process::exit(2);
    }

    let opts = match ServerOptions::from_script(script) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("cluster-server: {err}");
            process::exit(1);
        }
    };

    match run_server(&opts) {
        Ok(code) => process::exit(code),
        Err(ServerError::AlreadyRunning(path)) => {
            eprintln!("cluster-server: already running (lock {})", path.display());
            process::exit(1);
        }
        Err(err) => {
            eprintln!("cluster-server: {err}");
            process::exit(1);
        }
    }
}

fn usage() -> &'static str {
    "Usage: cluster-server <topology.py>\n\
     \n\
     Singleton cluster entry. Runs the Python topology DSL script, validates\n\
     the logical JSON, and spawns cluster-arbiter with that topology.\n"
}
