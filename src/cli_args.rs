//! QEMU-shaped CLI flags (`-s`, `-S`, `-gdb`) plus an optional guest ELF path.

use std::net::SocketAddr;
use std::path::PathBuf;

use sim_kernel::{GdbBindError, parse_gdb_dev};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliArgs {
    pub elf: Option<PathBuf>,
    pub gdb: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    Help,
    Message(String),
}

impl From<GdbBindError> for CliError {
    fn from(err: GdbBindError) -> Self {
        Self::Message(err.0)
    }
}

pub fn parse_cli<I, S>(args: I) -> Result<CliArgs, CliError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut elf = None;
    let mut gdb = None;
    let mut iter = args.into_iter().peekable();
    // skip argv0
    let _ = iter.next();
    while let Some(arg) = iter.next() {
        let arg = arg.as_ref();
        match arg {
            "-h" | "--help" => return Err(CliError::Help),
            "-s" => {
                gdb = Some(parse_gdb_dev("tcp::1234")?);
            }
            "-S" => {}
            "-gdb" => {
                let Some(dev) = iter.next() else {
                    return Err(CliError::Message(
                        "-gdb requires a device (e.g. tcp::1234)".into(),
                    ));
                };
                gdb = Some(parse_gdb_dev(dev.as_ref())?);
            }
            other if other.starts_with('-') => {
                return Err(CliError::Message(format!("unknown option {other}")));
            }
            other => {
                if elf.is_some() {
                    return Err(CliError::Message("multiple ELF paths given".into()));
                }
                elf = Some(PathBuf::from(other));
            }
        }
    }
    Ok(CliArgs { elf, gdb })
}

pub fn usage(argv0: &str) -> String {
    format!(
        "Usage: {argv0} [-s] [-S] [-gdb tcp::PORT] [guest.elf]\n\
         \n\
         -s              short for -gdb tcp::1234\n\
         -S              freeze at startup (always the case; accepted for QEMU compatibility)\n\
         -gdb tcp::PORT  listen for a GDB client (REPL still shown)\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qemu_flags() {
        let args = parse_cli(["bin", "-s", "-S", "app.elf"]).unwrap();
        assert_eq!(args.elf.as_deref().unwrap().as_os_str(), "app.elf");
        assert_eq!(args.gdb.unwrap().port(), 1234);
    }

    #[test]
    fn parses_gdb_tcp() {
        let args = parse_cli(["bin", "-gdb", "tcp::2159"]).unwrap();
        assert!(args.elf.is_none());
        assert_eq!(args.gdb.unwrap().port(), 2159);
    }
}
