//! GDB RSP stub hosted by cluster-server (`127.0.0.1`).

mod arch;
mod target;

use std::io;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gdbstub::common::Signal;
use gdbstub::conn::{Connection, ConnectionExt};
use gdbstub::stub::run_blocking::{BlockingEventLoop, Event, WaitForStopReasonError};
use gdbstub::stub::{GdbStub, SingleThreadStopReason};
use gdbstub::target::Target;

use crate::contract::GdbSessionInfo;
use crate::inspect::{InspectRequest, inspect_rpc, inspect_sock_path};
use crate::server::Job;
use target::{ClusterGdbTarget, map_status_reason};

pub struct GdbSlot {
    pub board_id: String,
    pub port: u16,
    pub host: Ipv4Addr,
    pub user: String,
    pub connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    active: Arc<Mutex<Option<TcpStream>>>,
    thread: Option<JoinHandle<()>>,
}

impl GdbSlot {
    pub fn info(&self) -> GdbSessionInfo {
        GdbSessionInfo {
            board_id: self.board_id.clone(),
            host: self.host.to_string(),
            port: self.port,
            user: self.user.clone(),
            ssh_command: ssh_command(&self.user, self.host, self.port),
            connected: self.connected.load(Ordering::SeqCst),
        }
    }

    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = self.active.lock()
            && let Some(stream) = slot.take()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let _ = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)));
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for GdbSlot {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn ssh_command(user: &str, host: Ipv4Addr, port: u16) -> String {
    format!("ssh -L {port}:127.0.0.1:{port} {user}@{host}")
}

#[must_use]
pub fn process_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "user".into())
}

pub fn spawn_slot(
    tx: Sender<Job>,
    sim_id: String,
    iox_root: PathBuf,
    board_id: String,
    host: Ipv4Addr,
) -> io::Result<GdbSlot> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let connected = Arc::new(AtomicBool::new(false));
    let active = Arc::new(Mutex::new(None));
    let shutdown_t = shutdown.clone();
    let connected_t = connected.clone();
    let active_t = active.clone();
    let board_t = board_id.clone();
    let handle = thread::Builder::new()
        .name("gdb".into())
        .spawn(move || {
            gdb_accept_loop(
                tx,
                sim_id,
                iox_root,
                board_t,
                listener,
                shutdown_t,
                connected_t,
                active_t,
            );
        })
        .expect("gdb thread");
    Ok(GdbSlot {
        board_id,
        port,
        host,
        user: process_user(),
        connected,
        shutdown,
        active,
        thread: Some(handle),
    })
}

#[allow(clippy::too_many_arguments)]
fn gdb_accept_loop(
    tx: Sender<Job>,
    sim_id: String,
    iox_root: PathBuf,
    board_id: String,
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    active: Arc<Mutex<Option<TcpStream>>>,
) {
    let inspect = inspect_sock_path(&iox_root, &board_id);
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let _ = stream.set_nodelay(true);
                if let Ok(clone) = stream.try_clone()
                    && let Ok(mut slot) = active.lock()
                {
                    *slot = Some(clone);
                }
                connected.store(true, Ordering::SeqCst);
                let mut target =
                    ClusterGdbTarget::new(tx.clone(), sim_id.clone(), inspect.clone(), &board_id);
                let _ = target.clear_session_breakpoints();
                let gdb = GdbStub::new(stream);
                let _ = gdb.run_blocking::<ClusterGdbEventLoop>(&mut target);
                connected.store(false, Ordering::SeqCst);
                if let Ok(mut slot) = active.lock() {
                    *slot = None;
                }
                let _ = inspect_rpc(&inspect, &InspectRequest::ClearBreakpoints);
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
}

enum ClusterGdbEventLoop {}

impl BlockingEventLoop for ClusterGdbEventLoop {
    type Target = ClusterGdbTarget;
    type Connection = TcpStream;
    type StopReason = SingleThreadStopReason<u32>;

    fn wait_for_stop_reason(
        target: &mut ClusterGdbTarget,
        conn: &mut Self::Connection,
    ) -> Result<
        Event<SingleThreadStopReason<u32>>,
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
            match target.status_stop_reason() {
                Ok(Some(reason)) => {
                    let _ = conn.set_read_timeout(None);
                    return Ok(Event::TargetStopped(map_status_reason(Some(&reason))));
                }
                Ok(None) => {}
                Err(err) => return Err(WaitForStopReasonError::Target(err)),
            }
        }
    }

    fn on_interrupt(
        target: &mut ClusterGdbTarget,
    ) -> Result<Option<SingleThreadStopReason<u32>>, String> {
        target.host(crate::host::HostCommand::Stop)?;
        Ok(Some(SingleThreadStopReason::Signal(Signal::SIGINT)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_command_uses_loopback_forward_and_server_ip() {
        let cmd = ssh_command("soya", Ipv4Addr::new(192, 168, 1, 10), 45231);
        assert_eq!(cmd, "ssh -L 45231:127.0.0.1:45231 soya@192.168.1.10");
    }
}
