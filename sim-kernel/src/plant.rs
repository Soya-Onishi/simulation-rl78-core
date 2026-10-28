//! Thin plant wrapper: a [`Model`] stepped on a fixed virtual-time grid.

use crate::clock::{Tick, VirtualClock};
use crate::entity::{AdvanceCtx, Entity, EntityPoll};
use crate::virtual_time::set_global_virtual_time_ns;

/// Fixed-step plant model. I/O is latched by the implementation (wire sinks /
/// [`crate::OutPort`] drives); [`Self::step`] takes no port arguments.
pub trait Model: Send {
    /// Virtual time between [`Self::step`] calls. Must be constant for a given model.
    fn dt(&self) -> Tick;

    fn reset(&mut self);

    /// Advance the model one step. Read latched inputs; drive outputs.
    fn step(&mut self);
}

/// [`Entity`] that runs `M` at `M::dt` and otherwise only advances the clock.
pub struct Plant<M: Model> {
    model: M,
    clock: VirtualClock,
    /// Next virtual time at which [`Model::step`] runs.
    next_step_at: Tick,
}

impl<M: Model> Plant<M> {
    #[must_use]
    pub fn new(model: M) -> Self {
        let clock = VirtualClock::new();
        let next_step_at = clock.now();
        Self {
            model,
            clock,
            next_step_at,
        }
    }

    #[must_use]
    pub fn model(&self) -> &M {
        &self.model
    }

    pub fn model_mut(&mut self) -> &mut M {
        &mut self.model
    }

    #[must_use]
    pub fn clock(&self) -> &VirtualClock {
        &self.clock
    }

    #[must_use]
    pub fn next_step_at(&self) -> Tick {
        self.next_step_at
    }

    fn advance_clock(&mut self, delta: Tick) {
        if delta.is_zero() {
            return;
        }
        self.clock.advance(delta);
        set_global_virtual_time_ns(self.clock.now().0);
    }
}

impl<M: Model> Entity for Plant<M> {
    fn poll(&mut self, ctx: AdvanceCtx) -> EntityPoll {
        let now = self.clock.now();
        let dt = self.model.dt();
        if now < self.next_step_at {
            let target = now.saturating_add(ctx.budget).min(self.next_step_at);
            let delta = target.saturating_sub(now);
            if delta.is_zero() {
                return EntityPoll::NoProgress;
            }
            self.advance_clock(delta);
            return EntityPoll::Progressed;
        }
        // `now == next_step_at`: run the solver at this instant. Clock stays here;
        // later polls idle-advance toward `next_step_at + dt`.
        self.model.step();
        self.next_step_at = self.next_step_at.saturating_add(dt);
        EntityPoll::Progressed
    }

    fn reset(&mut self) {
        self.model.reset();
        self.next_step_at = self.clock.now();
    }

    fn now(&self) -> Tick {
        self.clock.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, Response};
    use crate::sim::{SimConfig, Simulator};
    use crate::stop::StopReason;

    struct Counter {
        dt: Tick,
        pub steps: u32,
    }

    impl Model for Counter {
        fn dt(&self) -> Tick {
            self.dt
        }

        fn reset(&mut self) {
            self.steps = 0;
        }

        fn step(&mut self) {
            self.steps += 1;
        }
    }

    fn plant_sim(dt: Tick) -> Simulator<Plant<Counter>> {
        Simulator::new(Plant::new(Counter { dt, steps: 0 }), SimConfig::default())
    }

    #[test]
    fn idle_advances_clock_without_stepping() {
        let mut sim = plant_sim(Tick(1_000));
        sim.command(Command::SetAllowed { tick: Tick(10_000) });
        sim.command(Command::Start);
        assert_eq!(sim.poll(), None);
        assert_eq!(sim.entity().model().steps, 1);
        assert_eq!(sim.entity().now(), Tick(0));
        sim.command(Command::SetAllowed { tick: Tick(200) });
        assert_eq!(sim.poll(), None);
        assert_eq!(sim.entity().now(), Tick(200));
        assert_eq!(sim.entity().model().steps, 1);
        assert_eq!(sim.entity().next_step_at(), Tick(1_000));
    }

    #[test]
    fn steps_when_now_equals_next_step_at() {
        let mut sim = plant_sim(Tick(1_000));
        sim.command(Command::SetAllowed { tick: Tick(10_000) });
        sim.command(Command::Start);
        assert_eq!(sim.poll(), None);
        assert_eq!(sim.entity().model().steps, 1);
        assert_eq!(sim.entity().now(), Tick(0));
        assert_eq!(sim.entity().next_step_at(), Tick(1_000));
    }

    #[test]
    fn steps_when_now_equals_allowed_ceiling() {
        let mut sim = plant_sim(Tick(1_000));
        sim.command(Command::SetAllowed { tick: Tick(0) });
        sim.command(Command::Start);
        assert!(!sim.waiting_on_allowed_ceiling());
        assert_eq!(sim.poll(), None);
        assert_eq!(sim.entity().model().steps, 1);
        assert_eq!(sim.entity().now(), Tick(0));
        assert_eq!(sim.entity().next_step_at(), Tick(1_000));
        assert_eq!(sim.poll(), None);
        assert_eq!(sim.entity().model().steps, 1);
    }

    #[test]
    fn reset_keeps_clock_and_reschedules_step() {
        let mut sim = plant_sim(Tick(1_000));
        sim.command(Command::SetAllowed { tick: Tick(10_000) });
        sim.command(Command::Start);
        let _ = sim.poll();
        let _ = sim.poll();
        assert_eq!(sim.entity().now(), Tick(1_000));
        sim.command(Command::Stop);
        sim.entity_mut().reset();
        assert_eq!(sim.entity().now(), Tick(1_000));
        assert_eq!(sim.entity().next_step_at(), Tick(1_000));
        assert_eq!(sim.entity().model().steps, 0);
    }

    #[test]
    fn step_command_stops_after_one_poll() {
        let mut sim = plant_sim(Tick(1_000));
        sim.command(Command::SetAllowed { tick: Tick(10_000) });
        assert_eq!(sim.command(Command::Step), Response::Started);
        assert_eq!(sim.poll(), Some(Response::Stopped(StopReason::Step)));
        assert_eq!(sim.entity().model().steps, 1);
    }

    #[test]
    fn inspect_is_unsupported() {
        let mut sim = plant_sim(Tick(1_000));
        assert_eq!(
            sim.command(Command::ReadReg {
                id: crate::cpu::RegId(0)
            }),
            Response::Error(crate::command::SimError::Unsupported)
        );
    }
}
