//! RL78 minimal board — cluster board binary with a fixed ROM/RAM map.
//!
//! Assembles [`rl78_core::minimal_machine`] and hands control to
//! [`sim_cluster::run_board`]. Firmware loading (if the topology lists an ELF)
//! is performed by the framework via [`sim_kernel::Machine::load_firmware`].

use std::env;
use std::process;

use rl78_core::{g23_machine, G23MachineConfig};
use sim_cluster::{BoardCliError, board_usage, parse_board_args, run_board};

fn main() {
    let mut argv = env::args();
    let prog = argv.next().unwrap_or_else(|| "rl78-minimal-board".into());
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

    if let Err(err) = run_board(opts, || g23_machine(G23MachineConfig::default())) {
        eprintln!("rl78-minimal-board: {err}");
        process::exit(1);
    }
}
