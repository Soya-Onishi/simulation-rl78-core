//! 1-bit level DAC: GPIO levels to a held analog voltage.
//!
//! [`DigitalLevel::High`] is 5 V, [`DigitalLevel::Low`] is 0 V, and
//! [`DigitalLevel::HiZ`] keeps the last driven voltage (0 V before the first
//! drive). There is no time constant; each digital `drive` converts immediately.
//! The analog terminal is a [`SourcePort`]; the board wires it to an [`OutPort`].

use std::sync::{Arc, Mutex};

use sim_kernel::{
    AnalogVoltage, Device, DeviceError, DigitalLevel, MemoryBus, Resettable, SourcePort, id_hash,
};

/// High-level output in microvolts (5 V).
pub const HIGH_UV: i64 = 5_000_000;

/// Converts a digital pad level to [`AnalogVoltage`] and drives an analog [`SourcePort`].
pub struct LevelDac {
    name: String,
    name_hash: u64,
    last: Arc<Mutex<AnalogVoltage>>,
    analog: SourcePort<AnalogVoltage>,
}

impl LevelDac {
    /// Register a DAC named `name`. Bind [`Self::source_mut`] before [`Self::callback`].
    ///
    /// # Errors
    ///
    /// `name` is empty.
    pub fn new(name: impl Into<String>) -> Result<Self, DeviceError> {
        let name = name.into();
        if name.is_empty() {
            return Err(DeviceError::EmptyName);
        }
        Ok(Self {
            name_hash: id_hash(&name),
            name,
            last: Arc::new(Mutex::new(AnalogVoltage(0))),
            analog: SourcePort::new(),
        })
    }

    /// Analog output. Bind a [`sim_kernel::Wire`] to this reference before driving.
    #[must_use]
    pub fn source_mut(&mut self) -> &mut SourcePort<AnalogVoltage> {
        &mut self.analog
    }

    /// Convert `level` and drive the analog source.
    pub fn drive(&self, level: DigitalLevel) -> AnalogVoltage {
        let v = convert(level, &self.last);
        self.analog.drive(v);
        v
    }

    /// Wire sink for a digital pad: convert and [`SourcePort::drive`] the analog output.
    ///
    /// Call this after the analog [`sim_kernel::Wire`] is bound, so the cloned
    /// source sees the same wire.
    #[must_use]
    pub fn callback(&self) -> impl Fn(&[DigitalLevel], usize) + Send + Sync + 'static {
        let analog = self.analog.clone();
        let last = Arc::clone(&self.last);
        move |values, changed| {
            if let Some(&level) = values.get(changed) {
                analog.drive(convert(level, &last));
            }
        }
    }
}

fn convert(level: DigitalLevel, last: &Mutex<AnalogVoltage>) -> AnalogVoltage {
    let mut g = last.lock().expect("dac last");
    let v = match level {
        DigitalLevel::High => AnalogVoltage(HIGH_UV),
        DigitalLevel::Low => AnalogVoltage(0),
        DigitalLevel::HiZ => *g,
    };
    *g = v;
    v
}

impl Resettable for LevelDac {
    fn reset(&mut self, _bus: &mut MemoryBus) {
        let v = *self.last.lock().expect("dac last");
        self.analog.drive(v);
    }
}

impl Device for LevelDac {
    fn name(&self) -> &str {
        &self.name
    }

    fn name_hash(&self) -> u64 {
        self.name_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{MemoryMapBuilder, OutPort, Wire};

    fn empty_bus() -> MemoryBus {
        MemoryMapBuilder::new().build().unwrap()
    }

    fn wired_dac() -> (LevelDac, OutPort<AnalogVoltage>) {
        let mut dac = LevelDac::new("dac0").unwrap();
        let analog = OutPort::new("dac0").unwrap();
        {
            let out = analog.clone();
            let _w = Wire::new()
                .source(dac.source_mut())
                .sink(move |values, changed| out.on_input(values, changed));
        }
        (dac, analog)
    }

    #[test]
    fn high_is_five_volts() {
        let (dac, analog) = wired_dac();
        assert_eq!(dac.drive(DigitalLevel::High), AnalogVoltage(HIGH_UV));
        assert_eq!(
            analog.pending().last().copied(),
            Some(AnalogVoltage(HIGH_UV))
        );
    }

    #[test]
    fn low_is_zero_volts() {
        let (dac, analog) = wired_dac();
        dac.drive(DigitalLevel::High);
        analog.clear_pending();
        assert_eq!(dac.drive(DigitalLevel::Low), AnalogVoltage(0));
        assert_eq!(analog.pending().last().copied(), Some(AnalogVoltage(0)));
    }

    #[test]
    fn hiz_holds_last_voltage() {
        let (dac, analog) = wired_dac();
        assert_eq!(dac.drive(DigitalLevel::HiZ), AnalogVoltage(0));
        dac.drive(DigitalLevel::High);
        analog.clear_pending();
        assert_eq!(dac.drive(DigitalLevel::HiZ), AnalogVoltage(HIGH_UV));
        assert_eq!(
            analog.pending().last().copied(),
            Some(AnalogVoltage(HIGH_UV))
        );
    }

    #[test]
    fn reset_redrives_held_voltage() {
        let (mut dac, analog) = wired_dac();
        dac.drive(DigitalLevel::High);
        analog.clear_pending();
        let mut bus = empty_bus();
        dac.reset(&mut bus);
        assert_eq!(
            analog.pending().last().copied(),
            Some(AnalogVoltage(HIGH_UV))
        );
        analog.clear_pending();
        assert_eq!(dac.drive(DigitalLevel::HiZ), AnalogVoltage(HIGH_UV));
        assert_eq!(
            analog.pending().last().copied(),
            Some(AnalogVoltage(HIGH_UV))
        );
    }
}
