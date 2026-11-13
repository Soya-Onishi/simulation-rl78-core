//! Thin plant wrapper: a [`Model`] stepped on a fixed virtual-time grid.

use std::collections::HashMap;

use crate::action::{ActionCatalogEntry, ActionFailure, Device, DeviceError};
use crate::bus::MemoryBus;
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
    devices: HashMap<u64, Box<dyn Device>>,
    device_order: Vec<u64>,
}

impl<M: Model> Plant<M> {
    #[must_use]
    pub fn new(model: M) -> Self {
        Self::with_devices(model, Vec::new()).expect("empty device list")
    }

    /// Like [`Self::new`], and retains `devices` for host actions / reset.
    ///
    /// # Errors
    ///
    /// Two devices hash to the same [`crate::id_hash`] value.
    pub fn with_devices(model: M, devices: Vec<Box<dyn Device>>) -> Result<Self, DeviceError> {
        let mut device_order = Vec::with_capacity(devices.len());
        let mut by_hash = HashMap::with_capacity(devices.len());
        for device in devices {
            let hash = device.name_hash();
            let name = device.name().to_string();
            if by_hash.insert(hash, device).is_some() {
                return Err(DeviceError::DuplicateDevice { name });
            }
            device_order.push(hash);
        }
        let clock = VirtualClock::new();
        let next_step_at = clock.now();
        Ok(Self {
            model,
            clock,
            next_step_at,
            devices: by_hash,
            device_order,
        })
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

    #[must_use]
    pub fn action_catalog(&self) -> Vec<ActionCatalogEntry> {
        let mut out = Vec::new();
        for hash in &self.device_order {
            let device = &self.devices[hash];
            for spec in device.action_specs() {
                out.push(ActionCatalogEntry {
                    device: device.name().to_string(),
                    device_hash: device.name_hash(),
                    action: spec.id.clone(),
                    action_hash: spec.hash,
                    description: spec.description.clone(),
                    params: spec.params.clone(),
                });
            }
        }
        out
    }

    pub fn poll_actions(&mut self, now: Tick) {
        for device in self.devices.values_mut() {
            device.poll(now);
        }
    }

    pub fn apply_action(
        &mut self,
        device_hash: u64,
        action_hash: u64,
        now: Tick,
        raw: &[(u64, f64)],
    ) -> Result<(), ActionFailure> {
        let Some(device) = self.devices.get_mut(&device_hash) else {
            return Err(ActionFailure::NotFoundDevice);
        };
        device.apply(action_hash, now, raw)
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
        let mut bus = MemoryBus::new();
        for device in self.devices.values_mut() {
            device.reset(&mut bus);
        }
        self.model.reset();
        self.next_step_at = self.clock.now();
    }

    fn now(&self) -> Tick {
        self.clock.now()
    }

    fn action_catalog(&self) -> Vec<ActionCatalogEntry> {
        Plant::action_catalog(self)
    }

    fn poll_actions(&mut self, now: Tick) {
        Plant::poll_actions(self, now);
    }

    fn apply_action(
        &mut self,
        device_hash: u64,
        action_hash: u64,
        now: Tick,
        raw: &[(u64, f64)],
    ) -> Result<(), ActionFailure> {
        Plant::apply_action(self, device_hash, action_hash, now, raw)
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
