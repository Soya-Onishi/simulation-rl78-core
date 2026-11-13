//! Host-settable scalar knob for plant models.

use std::sync::{Arc, Mutex};

use crate::action::{
    Action, ActionFailure, ActionSpec, Device, DeviceError, ParamSpec, Parameters, id_hash,
    prepare_parameters,
};
use crate::bus::MemoryBus;
use crate::clock::Tick;
use crate::reset::Resettable;

const SET_ID: &str = "set";
const VALUE_ID: &str = "value";

/// One `set` action that writes a clamped `f64` into shared storage.
pub struct Knob {
    name: String,
    name_hash: u64,
    value: Arc<Mutex<f64>>,
    initial: f64,
    set: SetValue,
}

struct SetValue {
    spec: ActionSpec,
    value: Arc<Mutex<f64>>,
    min: f64,
    max: f64,
}

impl Knob {
    /// Create a knob named `name` with inclusive `[min, max]` and `initial` value.
    ///
    /// # Errors
    ///
    /// Empty name, non-finite bounds, `min > max`, or `initial` outside the range.
    pub fn new(
        name: impl Into<String>,
        initial: f64,
        min: f64,
        max: f64,
        unit: Option<&str>,
        description: impl Into<String>,
    ) -> Result<Self, DeviceError> {
        let name = name.into();
        if name.is_empty() {
            return Err(DeviceError::EmptyName);
        }
        if !min.is_finite() || !max.is_finite() || !initial.is_finite() || min > max {
            return Err(DeviceError::BadConfig);
        }
        if initial < min || initial > max {
            return Err(DeviceError::BadConfig);
        }
        let value = Arc::new(Mutex::new(initial));
        let mut param = ParamSpec::new(VALUE_ID);
        param.unit = unit.map(str::to_string);
        param.min = Some(min);
        param.max = Some(max);
        param.default = Some(initial);
        let mut spec = ActionSpec::new(SET_ID, description);
        spec.params.push(param);
        Ok(Self {
            name_hash: id_hash(&name),
            name,
            value: Arc::clone(&value),
            initial,
            set: SetValue {
                spec,
                value,
                min,
                max,
            },
        })
    }

    /// Shared storage read by the plant model each step.
    #[must_use]
    pub fn value_handle(&self) -> Arc<Mutex<f64>> {
        Arc::clone(&self.value)
    }

    #[must_use]
    pub fn get(&self) -> f64 {
        *self.value.lock().expect("knob")
    }
}

impl Action for SetValue {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }

    fn execute(&mut self, _now: Tick, params: &Parameters) -> Result<(), ActionFailure> {
        let raw = params
            .get(id_hash(VALUE_ID))
            .ok_or(ActionFailure::BadParam)?;
        if !raw.is_finite() || raw < self.min || raw > self.max {
            return Err(ActionFailure::BadParam);
        }
        *self.value.lock().expect("knob") = raw;
        Ok(())
    }

    fn poll(&mut self, _now: Tick) {}

    fn busy(&self, _now: Tick) -> bool {
        false
    }

    fn reset_action(&mut self) {}
}

impl Resettable for Knob {
    fn reset(&mut self, _bus: &mut MemoryBus) {
        *self.value.lock().expect("knob") = self.initial;
    }
}

impl Device for Knob {
    fn name(&self) -> &str {
        &self.name
    }

    fn name_hash(&self) -> u64 {
        self.name_hash
    }

    fn apply(
        &mut self,
        action_hash: u64,
        now: Tick,
        raw: &[(u64, f64)],
    ) -> Result<(), ActionFailure> {
        if action_hash != self.set.spec.hash {
            return Err(ActionFailure::NotFoundAction);
        }
        let params = prepare_parameters(&self.set.spec, raw)?;
        self.set.execute(now, &params)
    }

    fn action_mut(&mut self, action_hash: u64) -> Option<&mut (dyn Action + '_)> {
        (action_hash == self.set.spec.hash).then_some(&mut self.set as &mut dyn Action)
    }

    fn action_specs(&self) -> Vec<ActionSpec> {
        vec![self.set.spec.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_rejects_out_of_range() {
        let mut knob = Knob::new("pressure", 1.0, 0.0, 2.0, Some("norm"), "set pressure").unwrap();
        let hash = id_hash(SET_ID);
        knob
            .apply(hash, Tick::ZERO, &[(id_hash(VALUE_ID), 1.5)])
            .unwrap();
        assert_eq!(knob.get(), 1.5);
        assert!(
            knob
                .apply(hash, Tick::ZERO, &[(id_hash(VALUE_ID), 2.5)])
                .is_err()
        );
    }
}
