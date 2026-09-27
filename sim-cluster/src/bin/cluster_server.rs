//! `cluster-server` — entry: run Python DSL, spawn `cluster-arbiter`.

use std::env;
use std::path::PathBuf;
use std::process;

use sim_cluster::{LogLevel, ServerOptions, run_server};

fn main() {
    let mut args = env::args().skip(1);
    let mut log_level = LogLevel::Info;
    let mut script: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                eprint!("{}", usage());
                process::exit(0);
            }
            "--log-level" => {
                let Some(label) = args.next() else {
                    eprintln!("cluster-server: --log-level requires a level");
                    eprint!("{}", usage());
                    process::exit(2);
                };
                let Some(level) = LogLevel::from_label(&label) else {
                    eprintln!("cluster-server: unknown log level {label}");
                    eprint!("{}", usage());
                    process::exit(2);
                };
                log_level = level;
            }
            other if other.starts_with('-') => {
                eprintln!("cluster-server: unknown argument {other}");
                eprint!("{}", usage());
                process::exit(2);
            }
            other => {
                if script.is_some() {
                    eprintln!("cluster-server: unexpected extra arguments");
                    eprint!("{}", usage());
                    process::exit(2);
                }
                script = Some(PathBuf::from(other));
            }
        }
    }
    let Some(script) = script else {
        eprint!("{}", usage());
        process::exit(2);
    };

    let mut opts = match ServerOptions::from_script(script) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("cluster-server: {err}");
            process::exit(1);
        }
    };
    opts.log_level = log_level;

    match run_server(&opts) {
        Ok(code) => process::exit(code),
        Err(err) => {
            eprintln!("cluster-server: {err}");
            process::exit(1);
        }
    }
}

fn usage() -> &'static str {
    "Usage: cluster-server [--log-level <error|warn|info|debug|trace>] <topology.py>\n\
     \n\
     Cluster entry. Runs the Python topology DSL script, validates the logical\n\
     JSON, and spawns cluster-arbiter with that topology. Node and arbiter logs\n\
     are printed here. Default log level is info (error, warn, and info).\n"
}
