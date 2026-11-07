//! GDB RSP stub hosted by the web layer (`127.0.0.1`).

mod arch;
mod target;

use std::io;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gdbstub::common::Signal;
use gdbstub::conn::{Connection, ConnectionExt};
use gdbstub::stub::run_blocking::{BlockingEventLoop, Event, WaitForStopReasonError};
use gdbstub::stub::{GdbStub, SingleThreadStopReason};
use gdbstub::target::Target;

use crate::backend::SimulationBackend;
use crate::contract::GdbSessionInfo;
use crate::ipc::HashedId;
use target::{ClusterGdbTarget, map_status_reason};

pub struct GdbSlot {
    pub board_id: HashedId<u64>,
    pub port: u16,
    pub host: String,
    pub user: String,
    pub connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    active: Arc<Mutex<Option<TcpStream>>>,
    thread: Option<JoinHandle<()>>,
}

impl GdbSlot {
    pub fn info(&self) -> GdbSessionInfo {
        GdbSessionInfo {
            board_id: self.board_id,
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            connected: self.connected.load(Ordering::SeqCst),
        }
    }

    #[must_use]
    pub fn ssh_command(&self) -> String {
        format!(
            "ssh -L {0}:127.0.0.1:{0} {1}@{2}",
            self.port, self.user, self.host
        )
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

#[must_use]
pub fn process_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "user".into())
}

pub fn spawn_slot(
    backend: Arc<dyn SimulationBackend>,
    sim_id: String,
    board_id: HashedId<u64>,
    host: String,
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
    let board_t = board_id;
    let handle = thread::Builder::new()
        .name("gdb".into())
        .spawn(move || {
            gdb_accept_loop(
                backend,
                sim_id,
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

fn gdb_accept_loop(
    backend: Arc<dyn SimulationBackend>,
    sim_id: String,
    board_id: HashedId<u64>,
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    active: Arc<Mutex<Option<TcpStream>>>,
) {
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
                let mut target = ClusterGdbTarget::new(backend.clone(), sim_id.clone(), board_id);
                let _ = target.clear_session_breakpoints();
                let gdb = GdbStub::new(stream);
                let _ = gdb.run_blocking::<ClusterGdbEventLoop>(&mut target);
                connected.store(false, Ordering::SeqCst);
                if let Ok(mut slot) = active.lock() {
                    *slot = None;
                }
                let _ = target.clear_session_breakpoints();
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
        // Cap GetStatus traffic. Incoming bytes and an already-stopped guest
        // still return on this iteration; the sleep runs only when both are idle.
        const IDLE_POLL: Duration = Duration::from_millis(100);
        loop {
            match conn.peek() {
                Ok(Some(_)) => {
                    let byte = conn.read().map_err(WaitForStopReasonError::Connection)?;
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
                    return Ok(Event::TargetStopped(map_status_reason(Some(&reason))));
                }
                Ok(None) => {}
                Err(err) => return Err(WaitForStopReasonError::Target(err)),
            }
            thread::sleep(IDLE_POLL);
        }
    }

    fn on_interrupt(
        target: &mut ClusterGdbTarget,
    ) -> Result<Option<SingleThreadStopReason<u32>>, String> {
        target.stop()?;
        target.wait_stopped()?;
        Ok(Some(SingleThreadStopReason::Signal(Signal::SIGINT)))
    }
}
