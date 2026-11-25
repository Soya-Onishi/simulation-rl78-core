//! Simulation thread loop and state machine (#147 Phase 2 table).
//!
//! Guest / plant `poll` and the dataplane are not wired yet (Phase 3); only
//! the run state and the `host_ctrl` emergency stop are modelled.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use contract::{HostCtrl, NodeCommand, NodeCommandError, NodeCommandResult};

use crate::runtime::Routed;

/// `stop_generation` poll period.
const STOP_POLL_PERIOD: Duration = Duration::from_millis(1);

/// Apply routed commands and watch `stop_generation` until `rx` disconnects.
pub(crate) fn simulation_loop(host_ctrl: &HostCtrl, rx: &Receiver<Routed>, state: &AtomicU8) {
    let mut sim = Simulation::new(host_ctrl.stop_generation());
    loop {
        let received = rx.recv_timeout(STOP_POLL_PERIOD);
        // Consume bumps made before this command so a Start sent after an
        // emergency stop is not undone by that older stop.
        if sim.observe_stop_generation(host_ctrl.stop_generation()) {
            state.store(sim.state() as u8, Ordering::Release);
            log::info!("emergency stop: host_ctrl stop_generation advanced");
        }
        match received {
            Ok(Routed { command, reply }) => {
                let result = sim.apply(command);
                state.store(sim.state() as u8, Ordering::Release);
                let _ = reply.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Simulation run state as seen by Command.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimState {
    Stopped = 0,
    Running = 1,
}

impl SimState {
    #[must_use]
    pub(crate) fn from_u8(raw: u8) -> Self {
        if raw == Self::Running as u8 {
            Self::Running
        } else {
            Self::Stopped
        }
    }
}

pub(crate) struct Simulation {
    state: SimState,
    last_stop_generation: u64,
}

impl Simulation {
    #[must_use]
    pub(crate) fn new(stop_generation: u64) -> Self {
        Self {
            state: SimState::Stopped,
            last_stop_generation: stop_generation,
        }
    }

    #[must_use]
    pub(crate) fn state(&self) -> SimState {
        self.state
    }

    /// Apply a control command routed by the IPC thread.
    pub(crate) fn apply(&mut self, command: NodeCommand) -> NodeCommandResult {
        match command {
            NodeCommand::Start => {
                self.state = SimState::Running;
                NodeCommandResult::Success
            }
            NodeCommand::Stop | NodeCommand::Reset => {
                self.state = SimState::Stopped;
                NodeCommandResult::Success
            }
            NodeCommand::GdbStep
            | NodeCommand::GdbStepSync { .. }
            | NodeCommand::Inspect { .. }
            | NodeCommand::Action { .. } => NodeCommandResult::Error(NodeCommandError::Unavailable),
            NodeCommand::Shutdown | NodeCommand::ProbeSubscribe { .. } => {
                NodeCommandResult::Error(NodeCommandError::ImplFault)
            }
        }
    }

    /// Stop when `stop_generation` advanced since the last observation.
    ///
    /// Each bump is one stop event, so a later Start runs again. Returns
    /// `true` when this observation triggered the stop.
    pub(crate) fn observe_stop_generation(&mut self, stop_generation: u64) -> bool {
        if stop_generation <= self.last_stop_generation {
            return false;
        }
        self.last_stop_generation = stop_generation;
        self.state = SimState::Stopped;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::{HashedId, NodeCommandDto};

    #[test]
    fn start_stop_reset_follow_table() {
        let mut sim = Simulation::new(0);
        assert_eq!(sim.state(), SimState::Stopped);
        assert_eq!(sim.apply(NodeCommand::Start), NodeCommandResult::Success);
        assert_eq!(sim.state(), SimState::Running);
        assert_eq!(sim.apply(NodeCommand::Start), NodeCommandResult::Success);
        assert_eq!(sim.state(), SimState::Running);
        assert_eq!(sim.apply(NodeCommand::Stop), NodeCommandResult::Success);
        assert_eq!(sim.state(), SimState::Stopped);
        sim.apply(NodeCommand::Start);
        assert_eq!(sim.apply(NodeCommand::Reset), NodeCommandResult::Success);
        assert_eq!(sim.state(), SimState::Stopped);
    }

    #[test]
    fn unimplemented_commands_are_unavailable() {
        let mut sim = Simulation::new(0);
        sim.apply(NodeCommand::Start);
        let unavailable = NodeCommandResult::Error(NodeCommandError::Unavailable);
        assert_eq!(sim.apply(NodeCommand::GdbStep), unavailable);
        assert_eq!(
            sim.apply(NodeCommand::GdbStepSync { until_ns: 10 }),
            unavailable
        );
        assert_eq!(sim.apply(NodeCommand::inspect(&[1]).unwrap()), unavailable);
        let action = NodeCommandDto::Action {
            device: HashedId::new(1),
            action: HashedId::new(2),
            params: vec![],
        }
        .to_wire()
        .unwrap();
        assert_eq!(sim.apply(action), unavailable);
        assert_eq!(sim.state(), SimState::Running);
    }

    #[test]
    fn stop_generation_bump_stops_once_per_event() {
        let mut sim = Simulation::new(5);
        sim.apply(NodeCommand::Start);
        assert!(!sim.observe_stop_generation(5));
        assert_eq!(sim.state(), SimState::Running);
        assert!(sim.observe_stop_generation(6));
        assert_eq!(sim.state(), SimState::Stopped);

        sim.apply(NodeCommand::Start);
        assert!(!sim.observe_stop_generation(6));
        assert_eq!(sim.state(), SimState::Running);
    }
}
