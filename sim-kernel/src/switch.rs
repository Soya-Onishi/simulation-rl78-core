//! Board switch that holds a digital input for a virtual-time duration.

use std::collections::HashMap;

use crate::action::{
    Action, ActionFailure, ActionSpec, Device, DeviceError, ParamSpec, Parameters, id_hash,
};
use crate::bus::MemoryBus;
use crate::clock::{NS_PER_SEC, Tick};
use crate::reset::Resettable;
use crate::wiring::{DigitalLevel, SourcePort};

const HOLD_ID: &str = "hold";
const MILLISECONDS: &str = "milliseconds";
const NS_PER_MS: u64 = NS_PER_SEC / 1_000;

/// Maximum millisecond count that converts to virtual nanoseconds in `u64`.
pub const MAX_HOLD_MS: u64 = u64::MAX / NS_PER_MS;

/// Switch [`Device`] named by the board author. Its action is `hold`.
///
/// The switch owns the [`SourcePort`] that `hold` drives. Wire [`Self::source_mut`]
/// after `new`.
pub struct HoldSwitch {
    name: String,
    name_hash: u64,
    /// Actions keyed by [`id_hash`] of the action id.
    actions: HashMap<u64, Box<Hold>>,
    /// Registration order. Keys into [`Self::actions`], used only when order matters.
    action_order: Vec<u64>,
}

/// `hold`: drive the switch output high until a virtual-time deadline.
struct Hold {
    source: SourcePort<DigitalLevel>,
    spec: ActionSpec,
    /// Virtual time at which the pin returns to [`DigitalLevel::Low`].
    deadline: Option<Tick>,
}

impl HoldSwitch {
    /// Register a switch named `name` with a single `hold` action.
    ///
    /// # Errors
    ///
    /// `name` is empty.
    pub fn new(name: impl Into<String>) -> Result<Self, DeviceError> {
        let name = name.into();
        if name.is_empty() {
            return Err(DeviceError::EmptyName);
        }
        let hold = Hold::new(SourcePort::new());
        let hash = hold.spec().hash;
        let mut actions = HashMap::new();
        actions.insert(hash, Box::new(hold));
        Ok(Self {
            name_hash: id_hash(&name),
            name,
            actions,
            action_order: vec![hash],
        })
    }

    /// Port that `hold` drives. Bind a [`crate::wiring::Wire`] to this reference.
    #[must_use]
    pub fn source_mut(&mut self) -> &mut SourcePort<DigitalLevel> {
        let hash = id_hash(HOLD_ID);
        &mut self.actions.get_mut(&hash).expect("hold").source
    }

    #[must_use]
    pub fn action_mut(&mut self, action_hash: u64) -> Option<&mut (dyn Action + '_)> {
        Some(self.actions.get_mut(&action_hash)?.as_mut())
    }

    pub fn poll(&mut self, now: Tick) {
        for action in self.actions.values_mut() {
            action.poll(now);
        }
    }

    /// Look up `action_hash` and run it with `raw` `(name_hash, value)` pairs.
    pub fn apply(
        &mut self,
        action_hash: u64,
        now: Tick,
        raw: &[(u64, f64)],
    ) -> Result<(), ActionFailure> {
        let Some(action) = self.actions.get_mut(&action_hash) else {
            return Err(ActionFailure::NotFoundAction);
        };
        if action.busy(now) {
            return Err(ActionFailure::Busy);
        }
        let params = crate::action::prepare_parameters(action.spec(), raw)?;
        action.execute(now, &params)
    }
}

impl Hold {
    fn new(source: SourcePort<DigitalLevel>) -> Self {
        let mut milliseconds = ParamSpec::new(MILLISECONDS);
        milliseconds.unit = Some("ms".to_string());
        milliseconds.description = Some("押下を維持する仮想時間。整数のミリ秒".to_string());
        milliseconds.min = Some(1.0);
        let mut spec = ActionSpec::new(HOLD_ID, "指定した仮想時間だけスイッチを押す");
        spec.params.push(milliseconds);
        Self {
            source,
            spec,
            deadline: None,
        }
    }

    fn milliseconds_hash(&self) -> u64 {
        self.spec.params[0].name_hash
    }
}

impl Action for Hold {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }

    fn execute(&mut self, now: Tick, params: &Parameters) -> Result<(), ActionFailure> {
        let ms = params
            .get(self.milliseconds_hash())
            .ok_or(ActionFailure::BadParam)?;
        if !ms.is_finite() || ms < 1.0 || !is_whole(ms) {
            return Err(ActionFailure::BadParam);
        }
        let ms = ms as u64;
        if ms > MAX_HOLD_MS {
            return Err(ActionFailure::BadParam);
        }
        let duration = ms.saturating_mul(NS_PER_MS);
        let deadline = now
            .as_ns()
            .checked_add(duration)
            .ok_or(ActionFailure::BadParam)?;
        self.deadline = Some(Tick(deadline));
        self.source.drive(DigitalLevel::High);
        Ok(())
    }

    fn poll(&mut self, now: Tick) {
        let Some(deadline) = self.deadline else {
            return;
        };
        if now >= deadline {
            self.deadline = None;
            self.source.drive(DigitalLevel::Low);
        }
    }

    fn busy(&self, now: Tick) -> bool {
        self.deadline.is_some_and(|deadline| now < deadline)
    }

    fn reset_action(&mut self) {
        self.deadline = None;
        self.source.drive(DigitalLevel::Low);
    }
}

impl Resettable for HoldSwitch {
    fn reset(&mut self, _bus: &mut MemoryBus) {
        for action in self.actions.values_mut() {
            action.reset_action();
        }
    }
}

impl Device for HoldSwitch {
    fn name(&self) -> &str {
        &self.name
    }

    fn name_hash(&self) -> u64 {
        self.name_hash
    }

    fn poll(&mut self, now: Tick) {
        HoldSwitch::poll(self, now);
    }

    fn apply(
        &mut self,
        action_hash: u64,
        now: Tick,
        raw: &[(u64, f64)],
    ) -> Result<(), ActionFailure> {
        HoldSwitch::apply(self, action_hash, now, raw)
    }

    fn action_mut(&mut self, action_hash: u64) -> Option<&mut (dyn Action + '_)> {
        HoldSwitch::action_mut(self, action_hash)
    }

    fn action_specs(&self) -> Vec<ActionSpec> {
        self.action_order
            .iter()
            .map(|hash| self.actions[hash].spec().clone())
            .collect()
    }
}

fn is_whole(value: f64) -> bool {
    value.fract() == 0.0
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::action::id_hash;
    use crate::wiring::Wire;

    fn wired_switch(name: &str) -> (HoldSwitch, Arc<Mutex<Vec<DigitalLevel>>>) {
        let mut device = HoldSwitch::new(name).expect(name);
        let levels = levels_of(device.source_mut());
        (device, levels)
    }

    fn levels_of(source: &mut SourcePort<DigitalLevel>) -> Arc<Mutex<Vec<DigitalLevel>>> {
        let levels = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&levels);
        let _wire = Wire::new().source(source).sink(move |values, changed| {
            if let Some(level) = values.get(changed).copied() {
                recorded.lock().expect("levels").push(level);
            }
        });
        // `source` keeps the wire alive after `_wire` drops.
        let _ = _wire;
        levels
    }

    #[test]
    fn hold_drives_high_until_deadline_then_low() {
        let (mut device, levels) = wired_switch("sw1");
        let action = id_hash(HOLD_ID);
        let ms = id_hash(MILLISECONDS);
        device.apply(action, Tick(0), &[(ms, 2.0)]).expect("apply");
        assert!(device.action_mut(action).expect("hold").busy(Tick(0)));
        device.poll(Tick(2 * NS_PER_MS));
        assert!(
            !device
                .action_mut(action)
                .expect("hold")
                .busy(Tick(2 * NS_PER_MS))
        );
        let got = levels.lock().expect("levels").clone();
        assert_eq!(got, vec![DigitalLevel::High, DigitalLevel::Low]);
    }

    #[test]
    fn second_hold_while_busy_is_rejected() {
        let (mut device, _levels) = wired_switch("sw1");
        let action = id_hash(HOLD_ID);
        let ms = id_hash(MILLISECONDS);
        device.apply(action, Tick(0), &[(ms, 5.0)]).expect("first");
        let err = device.apply(action, Tick(1), &[(ms, 5.0)]).unwrap_err();
        assert_eq!(err, ActionFailure::Busy);
    }

    #[test]
    fn zero_and_fractional_milliseconds_are_rejected() {
        let (mut device, _levels) = wired_switch("sw1");
        let action = id_hash(HOLD_ID);
        let ms = id_hash(MILLISECONDS);
        assert_eq!(
            device.apply(action, Tick(0), &[(ms, 0.0)]).unwrap_err(),
            ActionFailure::BadParam
        );
        assert_eq!(
            device.apply(action, Tick(0), &[(ms, 1.5)]).unwrap_err(),
            ActionFailure::BadParam
        );
    }
}
