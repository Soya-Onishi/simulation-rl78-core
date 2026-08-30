//! GDB Remote Serial Protocol stub via [`gdbstub`].
//!
//! Front-ends still talk to the simulation only through [`crate::Command`] /
//! [`crate::Response`]. [`crate::Command::NotifyHalt`] is issued on guest stop
//! but remains a no-op until a multi-board arbiter exists.

mod arch;
mod target;

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::Duration;

use gdbstub::common::Signal;
use gdbstub::conn::{Connection, ConnectionExt};
use gdbstub::stub::run_blocking::{BlockingEventLoop, Event, WaitForStopReasonError};
use gdbstub::stub::{DisconnectReason, GdbStub, SingleThreadStopReason};
use gdbstub::target::Target;

use crate::command::Response;
use crate::sim::SimControl;

use target::{SimGdbTarget, map_stop_reason};

/// Failed to parse a QEMU-style `-gdb` device string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GdbBindError(pub String);

impl std::fmt::Display for GdbBindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for GdbBindError {}

/// Parse QEMU `-gdb` device forms used in v1: `tcp::PORT` and `tcp:HOST:PORT`.
///
/// `tcp::PORT` binds to `0.0.0.0` (all IPv4 interfaces), matching QEMU's empty-host
/// listen behavior. Loopback-only listen needs an explicit host (`tcp:127.0.0.1:PORT`).
pub fn parse_gdb_dev(dev: &str) -> Result<SocketAddr, GdbBindError> {
    let rest = dev
        .strip_prefix("tcp:")
        .ok_or_else(|| GdbBindError(format!("unsupported gdb device `{dev}` (want tcp::PORT)")))?;
    let addr = if let Some(port) = rest.strip_prefix(':') {
        format!("0.0.0.0:{port}")
    } else {
        rest.to_string()
    };
    addr.to_socket_addrs()
        .map_err(|err| GdbBindError(err.to_string()))?
        .next()
        .ok_or_else(|| GdbBindError(format!("could not resolve `{addr}`")))
}

/// Listen for a GDB client. Serves one connection at a time, then accepts again
/// until the simulation thread goes away.
pub fn listen_gdb(
    ctrl: SimControl,
    bind: SocketAddr,
) -> io::Result<(SocketAddr, thread::JoinHandle<()>)> {
    let listener = TcpListener::bind(bind)?;
    let local = listener.local_addr()?;
    listener.set_nonblocking(false)?;
    let handle = thread::Builder::new()
        .name("gdb".into())
        .spawn(move || gdb_accept_loop(ctrl, listener))
        .expect("failed to spawn gdb thread");
    Ok((local, handle))
}

fn gdb_accept_loop(ctrl: SimControl, listener: TcpListener) {
    while let Ok((stream, _)) = listener.accept() {
        let _ = stream.set_nodelay(true);
        let events = ctrl.subscribe();
        let mut target = SimGdbTarget::new(ctrl.clone(), events);
        let gdb = GdbStub::new(stream);
        match gdb.run_blocking::<SimGdbEventLoop>(&mut target) {
            Ok(
                DisconnectReason::Disconnect
                | DisconnectReason::TargetExited(_)
                | DisconnectReason::TargetTerminated(_)
                | DisconnectReason::Kill,
            ) => {}
            Err(_) => {}
        }
    }
}

enum SimGdbEventLoop {}

impl BlockingEventLoop for SimGdbEventLoop {
    type Target = SimGdbTarget;
    type Connection = TcpStream;
    type StopReason = SingleThreadStopReason<u64>;

    fn wait_for_stop_reason(
        target: &mut SimGdbTarget,
        conn: &mut Self::Connection,
    ) -> Result<
        Event<SingleThreadStopReason<u64>>,
        WaitForStopReasonError<
            <Self::Target as Target>::Error,
            <Self::Connection as Connection>::Error,
        >,
    > {
        let _ = conn.set_read_timeout(Some(Duration::from_millis(20)));
        loop {
            match conn.peek() {
                Ok(Some(_)) => {
                    let byte = conn.read().map_err(WaitForStopReasonError::Connection)?;
                    let _ = conn.set_read_timeout(None);
                    return Ok(Event::IncomingData(byte));
                }
                Ok(None) => {}
                Err(err)
                    if err.kind() == io::ErrorKind::WouldBlock
                        || err.kind() == io::ErrorKind::TimedOut => {}
                Err(err) => return Err(WaitForStopReasonError::Connection(err)),
            }

            match target.events_mut().recv_timeout(Duration::from_millis(20)) {
                Ok(Response::Stopped(reason)) => {
                    target.notify_halt(reason.clone());
                    let _ = conn.set_read_timeout(None);
                    return Ok(Event::TargetStopped(map_stop_reason(&reason)));
                }
                Ok(Response::Quit) => {
                    return Err(WaitForStopReasonError::Target("simulation quit"));
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
    }

    fn on_interrupt(
        target: &mut SimGdbTarget,
    ) -> Result<Option<SingleThreadStopReason<u64>>, <SimGdbTarget as Target>::Error> {
        target.request_stop()?;
        match target.wait_stopped(Duration::from_secs(2))? {
            Some(reason) => {
                target.notify_halt(reason.clone());
                Ok(Some(map_stop_reason(&reason)))
            }
            None => Ok(Some(SingleThreadStopReason::Signal(Signal::SIGINT))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::arch::GDB_REG_COUNT;
    use super::*;
    use crate::bus::MemoryBus;
    use crate::event::EventCtl;
    use crate::machine::Machine;
    use crate::sim::{SimConfig, spawn};
    use crate::stop::StopReason;
    use crate::testing::ScriptedCpu;
    use std::io::{Read, Write};
    use std::net::Shutdown;

    fn read_reply(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut b = [0u8; 1];
        loop {
            stream.read_exact(&mut b).unwrap();
            if b[0] == b'$' {
                break;
            }
            // Ignore ack '+' and other noise.
        }
        let mut buf = Vec::new();
        loop {
            stream.read_exact(&mut b).unwrap();
            if b[0] == b'#' {
                let mut c = [0u8; 2];
                stream.read_exact(&mut c).unwrap();
                let _ = Write::write_all(stream, b"+");
                return String::from_utf8(buf).unwrap();
            }
            buf.push(b[0]);
        }
    }

    fn send_cmd(stream: &mut TcpStream, body: &str) -> String {
        let mut csum: u8 = 0;
        for b in body.as_bytes() {
            csum = csum.wrapping_add(*b);
        }
        write!(stream, "${body}#{csum:02x}").unwrap();
        Write::flush(stream).unwrap();
        read_reply(stream)
    }

    #[test]
    fn parse_tcp_port_only() {
        let addr = parse_gdb_dev("tcp::2159").unwrap();
        assert_eq!(addr.port(), 2159);
        assert_eq!(addr.ip(), std::net::Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn parse_tcp_explicit_loopback() {
        let addr = parse_gdb_dev("tcp:127.0.0.1:2159").unwrap();
        assert_eq!(addr.port(), 2159);
        assert_eq!(addr.ip(), std::net::Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn qsupported_and_g_over_loopback() {
        let (ctrl, _events) = spawn(
            Machine::new(ScriptedCpu::nops(8), MemoryBus::new(), EventCtl::new()),
            SimConfig::default(),
        );
        let (addr, _h) = listen_gdb(ctrl.clone(), "127.0.0.1:0".parse().unwrap()).unwrap();
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let halt = send_cmd(&mut stream, "?");
        assert!(halt.starts_with('S') || halt.starts_with('T'), "{halt}");

        let reply = send_cmd(&mut stream, "qSupported:multiprocess+");
        assert!(reply.contains("vContSupported+"), "{reply}");

        let regs = send_cmd(&mut stream, "g");
        assert_eq!(regs.len(), GDB_REG_COUNT * 8, "{regs}");
        assert!(regs.chars().all(|c| c.is_ascii_hexdigit()), "{regs}");

        // Detach without Kill (`k` tears down the stub immediately).
        let _ = send_cmd(&mut stream, "D");
        let _ = stream.shutdown(Shutdown::Both);
        let _ = ctrl.quit();
    }

    #[test]
    fn map_stop_reason_covers_kernel_stops() {
        let _ = map_stop_reason(&StopReason::Step);
        let _ = map_stop_reason(&StopReason::ExternalStop);
        let _ = map_stop_reason(&StopReason::Halt);
    }
}
