//! `cluster-node` — transitional board process (IdleCpu via board framework).

use std::env;
use std::process;

use sim_cluster::{BoardCliError, board_usage, parse_board_args, run_node};

fn main() {
    let mut argv = env::args();
    let prog = argv.next().unwrap_or_else(|| "cluster-node".into());
    let opts = match parse_board_args(&prog, argv) {
        Ok(opts) => opts,
        Err(BoardCliError::Help) => {
            eprint!("{}", board_usage(&prog));
            process::exit(0);
        }
        Err(BoardCliError::Message(msg)) => {
            eprintln!("{msg}");
            eprint!("{}", board_usage(&prog));
            process::exit(2);
        }
    };

    if let Err(err) = run_node(opts) {
        eprintln!("cluster-node: {err}");
        process::exit(1);
    }
}
