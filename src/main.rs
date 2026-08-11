//! Thin CLI around the simulation library.
//!
//! The process owns two threads: this REPL and the simulation thread. The CLI
//! only parses `start` / `stop` / `quit` and prints responses.

use std::io::{self, BufRead, Write};
use std::thread;

use rl78_core::{MinimalMachineConfig, minimal_machine};
use sim_kernel::{Command, spawn};

fn main() {
    let machine = minimal_machine(MinimalMachineConfig::default());
    let (ctrl, events) = spawn(machine, sim_kernel::SimConfig::default());

    let printer = thread::Builder::new()
        .name("cli-events".into())
        .spawn(move || {
            while let Ok(response) = events.recv() {
                println!("{response}");
                if matches!(response, sim_kernel::Response::Quit) {
                    break;
                }
            }
        })
        .expect("failed to spawn CLI event printer");

    println!("simulation-rl78-core  commands: start | stop | quit");
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        match parse_command(&line) {
            ParseResult::Command(cmd) => {
                let quitting = matches!(cmd, Command::Quit);
                if ctrl.send(cmd).is_err() {
                    eprintln!("simulation thread has exited");
                    break;
                }
                if quitting {
                    break;
                }
            }
            ParseResult::Empty => {}
            ParseResult::Unknown(token) => {
                eprintln!("unknown command: {token}  (start | stop | quit)");
            }
        }
        let _ = stdout.flush();
    }

    let _ = ctrl.quit();
    let _ = printer.join();
}

#[derive(Debug, PartialEq, Eq)]
enum ParseResult<'a> {
    Command(Command),
    Empty,
    Unknown(&'a str),
}

fn parse_command(line: &str) -> ParseResult<'_> {
    let token = line.trim();
    if token.is_empty() {
        return ParseResult::Empty;
    }
    match token {
        "start" => ParseResult::Command(Command::Start),
        "stop" => ParseResult::Command(Command::Stop),
        "quit" => ParseResult::Command(Command::Quit),
        other => ParseResult::Unknown(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_lifecycle_commands() {
        assert_eq!(
            parse_command(" start "),
            ParseResult::Command(Command::Start)
        );
        assert_eq!(parse_command("stop"), ParseResult::Command(Command::Stop));
        assert_eq!(parse_command("quit"), ParseResult::Command(Command::Quit));
        assert_eq!(parse_command("  "), ParseResult::Empty);
        assert_eq!(parse_command("help"), ParseResult::Unknown("help"));
    }
}
