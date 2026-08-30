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
    /// Cap on virtual time advanced per [`Simulator::poll`].
    pub max_quantum: Tick,
    /// Virtual time charged per retired instruction (icount scaling).
    ///
    /// Milestone 1 defaults to `Tick(1)` (1 insn = 1 ns). Real MCU timing
    /// models can raise this without changing the event/timer API.
    pub ns_per_instruction: Tick,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            max_quantum: DEFAULT_MAX_QUANTUM,
            ns_per_instruction: Tick(1),
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
    /// When true, the next quantum is capped to one instruction and then stops.
    step_once: bool,
}

impl<C: Cpu> Simulator<C> {
    #[must_use]
    pub fn new(machine: Machine<C>, cfg: SimConfig) -> Self {
        Self {
            machine,
            state: SimState::Stopped,
            cfg,
            step_once: false,
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
                self.step_once = false;
                self.state = SimState::Running;
                Response::Started
            }
            Command::Step => {
                self.step_once = true;
                self.state = SimState::Running;
                Response::Started
            }
            Command::Stop => {
                self.step_once = false;
                self.state = SimState::Stopped;
                Response::Stopped(StopReason::ExternalStop)
            }
            Command::Quit => {
                self.state = SimState::Quit;
                Response::Quit
            }
            Command::NotifyHalt { reason: _ } => {
                // TODO(multi-board): when wired from the kernel stop path, forward
                // debugger/cluster-relevant stops over IPC and wait for cluster
                // ack. Do not cluster-halt on guest-local [`StopReason::Halt`]
                // (RL78 STOP / WFI). GDB must not own this hook.
                Response::Inspect(InspectResult::Ok)
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

        let ns_per_insn = self.cfg.ns_per_instruction.max(Tick(1));
        let budget_ns = {
            let now = self.machine.clock().now();
            self.machine
                .events_mut()
                .next_deadline()
                .unwrap_or(Tick::MAX)
                .saturating_sub(now)
                .min(self.cfg.max_quantum)
        };
        let mut max_instructions = budget_ns
            .saturating_div(ns_per_insn)
            .min(u64::from(u32::MAX)) as u32;
        if self.step_once {
            max_instructions = 1;
        }
        if max_instructions == 0 {
            // Less than one instruction remains before the next deadline (or the
            // quantum cap). Advancing by that remainder lets due events fire;
            // returning without advancing would spin forever while Running.
            if budget_ns.is_zero() {
                return None;
            }
            self.machine.advance_clock(budget_ns);
            return self.fire_due_events();
        }

        let result = {
            let (cpu, _, _) = self.machine.parts_mut();
            cpu.run_quantum(max_instructions)
        };
        if result.instructions == 0 && result.stop.is_none() {
            self.step_once = false;
            self.state = SimState::Stopped;
            return Some(Response::Stopped(StopReason::Halt));
        }
        let elapsed = ns_per_insn.saturating_mul(result.instructions);
        self.machine.advance_clock(elapsed);

        if let Some(access) = self.machine.bus_mut().take_trap() {
            self.step_once = false;
            self.state = SimState::Stopped;
            return Some(Response::Stopped(StopReason::Unmapped {
                addr: access.addr,
                write: access.write,
            }));
        }

        if let Some(stop) = result.stop {
            self.step_once = false;
            self.state = SimState::Stopped;
            return Some(Response::Stopped(stop));
        }

        if self.step_once {
            self.step_once = false;
            self.state = SimState::Stopped;
            return Some(Response::Stopped(StopReason::Step));
        }

        self.fire_due_events()
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
                self.step_once = false;
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
            Command::Start
            | Command::Step
            | Command::Stop
            | Command::Quit
            | Command::NotifyHalt { .. } => {
                unreachable!("lifecycle commands are handled in command()")
            }
        }
    }

    fn sync_breakpoints(&mut self) {
        let (cpu, _, breakpoints) = self.machine.parts_mut();
        let snapshot = breakpoints.as_slice().to_vec();
        cpu.sync_breakpoints(&snapshot);
    }
}

/// Send side used by CLI / GDB. Cheap to clone; only the original from
/// [`spawn`] sends [`Command::Quit`] on drop.
pub struct SimControl {
    tx: mpsc::Sender<Command>,
    fanout: ResponseFanout,
    quit_on_drop: bool,
}

impl Clone for SimControl {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            fanout: self.fanout.clone(),
            quit_on_drop: false,
        }
    }
}

#[derive(Clone)]
struct ResponseFanout {
    txs: std::sync::Arc<std::sync::Mutex<Vec<mpsc::Sender<Response>>>>,
}

impl ResponseFanout {
    fn new() -> Self {
        Self {
            txs: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn subscribe(&self) -> SimEvents {
        let (tx, rx) = mpsc::channel();
        self.txs.lock().expect("response fanout").push(tx);
        SimEvents { rx }
    }

    fn send(&self, rsp: Response) -> bool {
        let mut txs = self.txs.lock().expect("response fanout");
        txs.retain(|tx| tx.send(rsp.clone()).is_ok());
        !txs.is_empty()
    }
}

impl SimControl {
    pub fn send(&self, cmd: Command) -> Result<(), SendError<Command>> {
        self.tx.send(cmd)
    }

    #[must_use]
    pub fn subscribe(&self) -> SimEvents {
        self.fanout.subscribe()
    }

    pub fn start(&self) -> Result<(), SendError<Command>> {
        self.send(Command::Start)
    }

    pub fn step(&self) -> Result<(), SendError<Command>> {
        self.send(Command::Step)
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
        if self.quit_on_drop {
            let _ = self.tx.send(Command::Quit);
        }
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
    let fanout = ResponseFanout::new();
    let events = fanout.subscribe();
    let thread_fanout = fanout.clone();
    thread::Builder::new()
        .name("sim".into())
        .spawn(move || sim_thread(machine, cfg, cmd_rx, thread_fanout))
        .expect("failed to spawn simulation thread");
    (
        SimControl {
            tx: cmd_tx,
            fanout,
            quit_on_drop: true,
        },
        events,
    )
}

fn sim_thread<C: Cpu>(
    machine: Machine<C>,
    cfg: SimConfig,
    cmd_rx: mpsc::Receiver<Command>,
    fanout: ResponseFanout,
) {
    let mut sim = Simulator::new(machine, cfg);
    loop {
        if sim.state() == SimState::Quit {
            break;
        }
        if sim.state() == SimState::Running {
            while let Ok(cmd) = cmd_rx.try_recv() {
                if !fanout.send(sim.command(cmd)) {
                    return;
                }
                if sim.state() == SimState::Quit {
                    return;
                }
            }
            if let Some(response) = sim.poll()
                && !fanout.send(response)
            {
                return;
            }
        } else {
            match cmd_rx.recv() {
                Ok(cmd) => {
                    if !fanout.send(sim.command(cmd)) {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }
}
