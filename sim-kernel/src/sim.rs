//! Quantum execution loop and threaded control API.

use std::sync::mpsc::{self, RecvError, RecvTimeoutError, SendError, TryRecvError};
use std::thread;
use std::time::Duration;

use crate::clock::Tick;
use crate::command::{Command, InspectResult, Response, SimError};
use crate::cpu::Cpu;
use crate::entity::{AdvanceCtx, Entity, EntityPoll};
use crate::machine::Machine;
use crate::stop::StopReason;

/// How far a single CPU quantum may run before the kernel re-checks commands.
pub const DEFAULT_MAX_QUANTUM: Tick = Tick(10_000);

/// Kernel run configuration.
#[derive(Clone, Debug)]
pub struct SimConfig {
    /// Cap on virtual time advanced per [`Simulator::poll`] for a [`Machine`].
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

/// Optional hook invoked at the start of each [`Simulator::poll`] quantum.
///
/// Used by multi-board nodes to drain B-lite receive mailboxes into
/// `InPort` on the simulation thread only.
pub trait BeforeQuantum: Send {
    fn before_quantum(&mut self);
}

/// In-thread simulator. [`spawn`] runs this on a dedicated thread.
pub struct Simulator<E: Entity> {
    entity: E,
    state: SimState,
    cfg: SimConfig,
    /// When true, the next poll completes as [`StopReason::Step`].
    step_once: bool,
    before_quantum: Option<Box<dyn BeforeQuantum>>,
    /// Multi-board ceiling: virtual time must not advance past this tick.
    allowed: Option<Tick>,
}

impl<E: Entity> Simulator<E> {
    #[must_use]
    pub fn new(entity: E, cfg: SimConfig) -> Self {
        Self {
            entity,
            state: SimState::Stopped,
            cfg,
            step_once: false,
            before_quantum: None,
            allowed: None,
        }
    }

    /// Install a quantum-entry hook (e.g. B-lite mailbox drain).
    pub fn set_before_quantum(&mut self, hook: impl BeforeQuantum + 'static) {
        self.before_quantum = Some(Box::new(hook));
    }

    /// Current virtual-time ceiling, if any.
    #[must_use]
    pub fn allowed(&self) -> Option<Tick> {
        self.allowed
    }

    /// True when Running and virtual time has passed the multi-board ceiling.
    ///
    /// `allowed == now` is not waiting: the entity may still step or fire events
    /// at that instant. The sim thread must park on the command channel when this
    /// is true; returning from [`Self::poll`] alone would busy-spin until the
    /// next `SetAllowed`.
    #[must_use]
    pub fn waiting_on_allowed_ceiling(&self) -> bool {
        if self.state != SimState::Running {
            return false;
        }
        let Some(allowed) = self.allowed else {
            return false;
        };
        self.entity.now() > allowed
    }

    #[must_use]
    pub fn state(&self) -> SimState {
        self.state
    }

    #[must_use]
    pub fn entity(&self) -> &E {
        &self.entity
    }

    pub fn entity_mut(&mut self) -> &mut E {
        &mut self.entity
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
            Command::SetAllowed { tick } => {
                self.allowed = Some(tick);
                Response::Inspect(InspectResult::Ok)
            }
            Command::NotifyHalt { reason: _ } => {
                // TODO(multi-board): when wired from the kernel stop path, forward
                // debugger/cluster-relevant stops over IPC and wait for cluster
                // ack. Do not cluster-halt on guest-local [`StopReason::Halt`]
                // (RL78 STOP / WFI). GDB must not own this hook.
                Response::Inspect(InspectResult::Ok)
            }
            other => {
                if self.state == SimState::Running {
                    return Response::Error(SimError::Running);
                }
                self.entity.inspect(other)
            }
        }
    }

    /// If running, advance the entity by one execution unit.
    pub fn poll(&mut self) -> Option<Response> {
        if self.state != SimState::Running {
            return None;
        }

        if let Some(hook) = self.before_quantum.as_mut() {
            hook.before_quantum();
        }

        let now = self.entity.now();
        if let Some(allowed) = self.allowed
            && now > allowed
        {
            return None;
        }

        let budget = match self.allowed {
            Some(allowed) => allowed.saturating_sub(now),
            None => Tick::MAX,
        };
        let ctx = AdvanceCtx {
            budget,
            max_quantum: self.cfg.max_quantum,
            step_once: self.step_once,
        };
        match self.entity.poll(ctx) {
            EntityPoll::NoProgress => None,
            EntityPoll::Progressed => {
                if self.step_once {
                    self.step_once = false;
                    self.state = SimState::Stopped;
                    Some(Response::Stopped(StopReason::Step))
                } else {
                    None
                }
            }
            EntityPoll::Done(Response::Stopped(reason)) => self.commit_stop(reason),
            EntityPoll::Done(Response::Error(err)) => {
                self.step_once = false;
                self.state = SimState::Stopped;
                Some(Response::Error(err))
            }
            EntityPoll::Done(other) => Some(other),
        }
    }

    /// Commit a stop reason from CPU or an event.
    ///
    /// [`StopReason::Halt`] is guest-local idle (WFI / STOP): keep
    /// [`SimState::Running`] and do **not** emit [`Response::Stopped`] (so CLI/GDB
    /// do not treat idle as a debugger stop). A pending [`Command::Step`] still
    /// completes as [`StopReason::Step`]. Other reasons clear Running.
    fn commit_stop(&mut self, stop: StopReason) -> Option<Response> {
        match stop {
            StopReason::Halt if self.step_once => {
                self.step_once = false;
                self.state = SimState::Stopped;
                Some(Response::Stopped(StopReason::Step))
            }
            StopReason::Halt => {
                self.step_once = false;
                None
            }
            other => {
                self.step_once = false;
                self.state = SimState::Stopped;
                Some(Response::Stopped(other))
            }
        }
    }
}

impl<C: Cpu> Simulator<Machine<C>> {
    #[must_use]
    pub fn machine(&self) -> &Machine<C> {
        self.entity()
    }

    pub fn machine_mut(&mut self) -> &mut Machine<C> {
        self.entity_mut()
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
            // Multi-board ceiling: wait for SetAllowed/Stop instead of pegging a core.
            if sim.waiting_on_allowed_ceiling() {
                match cmd_rx.recv() {
                    Ok(cmd) => {
                        if !fanout.send(sim.command(cmd)) {
                            return;
                        }
                    }
                    Err(_) => return,
                }
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
