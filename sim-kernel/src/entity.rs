//! Simulation entity: a [`crate::Machine`] or a [`crate::Plant`].

use crate::action::{ActionCatalogEntry, ActionFailure};
use crate::clock::Tick;
use crate::command::{Command, Response, SimError};
use crate::cpu::FirmwareError;

/// Budget for one [`Entity::poll`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvanceCtx {
    /// Virtual time the entity may still advance (`allowed - now`, or [`Tick::MAX`]).
    pub budget: Tick,
    /// CPU quantum cap. [`crate::Machine`] uses this; [`crate::Plant`] ignores it.
    pub max_quantum: Tick,
    /// When true, [`crate::Machine`] retires at most one instruction.
    pub step_once: bool,
}

/// Result of one [`Entity::poll`].
#[derive(Debug)]
pub enum EntityPoll {
    /// No time advanced and no model/CPU work (retry on the next poll).
    NoProgress,
    /// One execution unit completed without a stop.
    Progressed,
    /// Stop or control-plane error.
    Done(Response),
}

/// Something [`crate::Simulator`] can start, poll, and reset.
pub trait Entity: Send {
    /// Advance one execution unit. `budget` is time remaining before the ceiling.
    fn poll(&mut self, ctx: AdvanceCtx) -> EntityPoll;

    /// Hold-reset. Must not zero virtual time.
    fn reset(&mut self);

    /// Current virtual time.
    fn now(&self) -> Tick;

    /// Inspect / mutate while stopped. Default: not supported.
    fn inspect(&mut self, cmd: Command) -> Response {
        let _ = cmd;
        Response::Error(SimError::Unsupported)
    }

    /// Load firmware bytes. Default: this entity has no firmware.
    fn load_firmware(&mut self, image: &[u8]) -> Result<(), FirmwareError> {
        let _ = image;
        Err(FirmwareError::Unsupported(
            "this entity does not load firmware",
        ))
    }

    /// Host-invocable actions. Empty for a plant.
    #[must_use]
    fn action_catalog(&self) -> Vec<ActionCatalogEntry> {
        Vec::new()
    }

    /// Release action effects whose deadline has arrived.
    fn poll_actions(&mut self, now: Tick) {
        let _ = now;
    }

    /// Apply one host action.
    fn apply_action(
        &mut self,
        device_hash: u64,
        action_hash: u64,
        now: Tick,
        raw: &[(u64, f64)],
    ) -> Result<(), ActionFailure> {
        let _ = (device_hash, action_hash, now, raw);
        Err(ActionFailure::NotFoundDevice)
    }
}
