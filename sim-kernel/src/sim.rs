//! Quantum execution loop and threaded control API.

use std::sync::mpsc::{self, RecvError, RecvTimeoutError, SendError, TryRecvError};
use std::thread;
use std::time::Duration;

use crate::clock::Tick;
use crate::command::{Command, InspectResult, Response, SimError};
use crate::cpu::Cpu;
use crate::event::EventCtx;
use crate::machine::Machine;
use crate::stop::StopReason;

/// How far a single CPU quantum may run before the kernel re-checks commands.
pub const DEFAULT_MAX_QUANTUM: Tick = Tick(10_000);

/// Kernel run configuration.
#[derive(Clone, Debug)]
pub struct SimConfig {
    pub max_quantum: Tick,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            max_quantum: DEFAULT_MAX_QUANTUM,
        }
    }
}

/// High-level session state. Guest mutation only happens while `Running`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimState {
    Stopped,
    Running,
    Quit,
}

/// In-thread simulator. [`spawn`] runs this on a dedicated thread.
pub struct Simulator<C: Cpu> {
    machine: Machine<C>,
    state: SimState,
    cfg: SimConfig,
}

impl<C: Cpu> Simulator<C> {
    #[must_use]
    pub fn new(machine: Machine<C>, cfg: SimConfig) -> Self {
        Self {
            machine,
            state: SimState::Stopped,
            cfg,
        }
    }

    #[must_use]
    pub fn state(&self) -> SimState {
        self.state
    }

    #[must_use]
    pub fn machine(&self) -> &Machine<C> {
        &self.machine
    }

    pub fn machine_mut(&mut self) -> &mut Machine<C> {
        &mut self.machine
    }

    /// Apply a control-plane command.
    pub fn command(&mut self, cmd: Command) -> Response {
        if self.state == SimState::Quit {
            return Response::Quit;
        }
        match cmd {
            Command::Start => {
                self.state = SimState::Running;
                Response::Started
            }
            Command::Stop => {
                self.state = SimState::Stopped;
                Response::Stopped(StopReason::ExternalStop)
            }
            Command::Quit => {
                self.state = SimState::Quit;
                Response::Quit
            }
            other => self.inspect(other),
        }
    }

    /// If running, fire due events and execute one CPU quantum.
    pub fn poll(&mut self) -> Option<Response> {
        if self.state != SimState::Running {
            return None;
        }

        if let Some(response) = self.fire_due_events() {
            return Some(response);
        }

        let max_quantum = self.cfg.max_quantum;
        let quantum = {
            let (_, _, clock, events, _) = self.machine.parts_mut();
            let now = clock.now();
            events
                .next_deadline()
                .unwrap_or(Tick::MAX)
                .saturating_sub(now)
                .min(max_quantum)
        };
        if quantum.is_zero() {
            // An event is due at `now` but was not consumed; avoid a spin.
            return None;
        }

        let result = {
            let (cpu, bus, _, _, _) = self.machine.parts_mut();
            cpu.run_quantum(bus, quantum)
        };
        if result.ticks.is_zero() && result.stop.is_none() {
            self.state = SimState::Stopped;
            return Some(Response::Stopped(StopReason::Halt));
        }
        self.machine.clock_mut().advance(result.ticks);

        if let Some(access) = self.machine.bus_mut().take_trap() {
            self.state = SimState::Stopped;
            return Some(Response::Stopped(StopReason::Unmapped {
                addr: access.addr,
                write: access.write,
            }));
        }

        if let Some(stop) = result.stop {
            self.state = SimState::Stopped;
            return Some(Response::Stopped(stop));
        }

        let pc = self.machine.cpu().pc();
        if let Some(id) = self.machine.breakpoints().hit_at(pc) {
            self.state = SimState::Stopped;
            return Some(Response::Stopped(StopReason::Breakpoint { id }));
        }

        None
    }

    fn fire_due_events(&mut self) -> Option<Response> {
        loop {
            let now = self.machine.clock().now();
            let Some((_id, mut event)) = self.machine.events_mut().pop_due(now) else {
                break;
            };
            let stop = {
                let mut ctx = EventCtx {
                    now,
                    bus: self.machine.bus_mut(),
                    stop: None,
                };
                event.fire(&mut ctx);
                ctx.stop
            };
            if let Some(stop) = stop {
                self.state = SimState::Stopped;
                return Some(Response::Stopped(stop));
            }
        }
        None
    }

    fn inspect(&mut self, cmd: Command) -> Response {
        if self.state == SimState::Running {
            return Response::Error(SimError::Running);
        }
        match cmd {
            Command::ReadReg { id } => match self.machine.read_reg(id) {
                Ok(value) => Response::Inspect(InspectResult::Reg { id, value }),
                Err(err) => Response::Error(err),
            },
            Command::WriteReg { id, value } => match self.machine.write_reg(id, value) {
                Ok(()) => Response::Inspect(InspectResult::Ok),
                Err(err) => Response::Error(err),
            },
            Command::ReadMem { addr, len } => {
                let mut data = vec![0u8; len as usize];
                match self.machine.read_mem(addr, &mut data) {
                    Ok(()) => Response::Inspect(InspectResult::Mem { addr, data }),
                    Err(err) => Response::Error(SimError::Bus(err)),
                }
            }
            Command::WriteMem { addr, data } => match self.machine.write_mem(addr, &data) {
                Ok(()) => Response::Inspect(InspectResult::Ok),
                Err(err) => Response::Error(SimError::Bus(err)),
            },
            Command::AddBreakpoint { addr } => {
                let id = self.machine.breakpoints_mut().insert(addr);
                self.sync_breakpoints();
                Response::Inspect(InspectResult::Breakpoint { id })
            }
            Command::RemoveBreakpoint { id } => {
                if self.machine.breakpoints_mut().remove(id) {
                    self.sync_breakpoints();
                    Response::Inspect(InspectResult::Ok)
                } else {
                    Response::Error(SimError::UnknownBreakpoint(id))
                }
            }
            Command::Start | Command::Stop | Command::Quit => {
                unreachable!("lifecycle commands are handled in command()")
            }
        }
    }

    fn sync_breakpoints(&mut self) {
        let (cpu, _, _, _, breakpoints) = self.machine.parts_mut();
        let snapshot = breakpoints.as_slice().to_vec();
        cpu.sync_breakpoints(&snapshot);
    }
}

/// Send side used by CLI / future GDB.
pub struct SimControl {
    tx: mpsc::Sender<Command>,
}

impl SimControl {
    pub fn send(&self, cmd: Command) -> Result<(), SendError<Command>> {
        self.tx.send(cmd)
    }

    pub fn start(&self) -> Result<(), SendError<Command>> {
        self.send(Command::Start)
    }

    pub fn stop(&self) -> Result<(), SendError<Command>> {
        self.send(Command::Stop)
    }

    pub fn quit(&self) -> Result<(), SendError<Command>> {
        self.send(Command::Quit)
    }
}

impl Drop for SimControl {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
    }
}

/// Receive side for simulation notifications.
pub struct SimEvents {
    rx: mpsc::Receiver<Response>,
}

impl SimEvents {
    pub fn recv(&self) -> Result<Response, RecvError> {
        self.rx.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Response, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    pub fn try_recv(&self) -> Result<Response, TryRecvError> {
        self.rx.try_recv()
    }
}

/// Run `machine` on a dedicated simulation thread.
#[must_use]
pub fn spawn<C: Cpu + 'static>(machine: Machine<C>, cfg: SimConfig) -> (SimControl, SimEvents) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (rsp_tx, rsp_rx) = mpsc::channel();
    thread::Builder::new()
        .name("sim".into())
        .spawn(move || sim_thread(machine, cfg, cmd_rx, rsp_tx))
        .expect("failed to spawn simulation thread");
    (SimControl { tx: cmd_tx }, SimEvents { rx: rsp_rx })
}

fn sim_thread<C: Cpu>(
    machine: Machine<C>,
    cfg: SimConfig,
    cmd_rx: mpsc::Receiver<Command>,
    rsp_tx: mpsc::Sender<Response>,
) {
    let mut sim = Simulator::new(machine, cfg);
    loop {
        if sim.state() == SimState::Quit {
            break;
        }
        if sim.state() == SimState::Running {
            while let Ok(cmd) = cmd_rx.try_recv() {
                if rsp_tx.send(sim.command(cmd)).is_err() {
                    return;
                }
                if sim.state() == SimState::Quit {
                    return;
                }
            }
            if let Some(response) = sim.poll()
                && rsp_tx.send(response).is_err()
            {
                return;
            }
        } else {
            match cmd_rx.recv() {
                Ok(cmd) => {
                    if rsp_tx.send(sim.command(cmd)).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }
}
