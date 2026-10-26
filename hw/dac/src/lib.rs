//! 1-bit level DAC: GPIO levels to a held analog voltage.
//!
//! [`DigitalLevel::High`] is 5 V, [`DigitalLevel::Low`] is 0 V, and
//! [`DigitalLevel::HiZ`] keeps the last driven voltage (0 V before the first
//! drive). There is no time constant; each digital `drive` converts immediately.

use std::sync::{Arc, Mutex};

use sim_kernel::{
    AnalogVoltage, Device, DigitalLevel, MemoryBus, OutPort, PortError, Resettable, id_hash,
};

/// High-level output in microvolts (5 V).
pub const HIGH_UV: i64 = 5_000_000;

/// Converts a digital pad level to [`AnalogVoltage`] and drives an analog [`OutPort`].
pub struct LevelDac {
    name: String,
    name_hash: u64,
    last: Arc<Mutex<AnalogVoltage>>,
    analog: OutPort<AnalogVoltage>,
}

impl LevelDac {
    pub fn new(name: impl Into<String>) -> Result<Self, PortError> {
        let name = name.into();
        let analog = OutPort::new(name.clone())?;
        Ok(Self {
            name_hash: id_hash(&name),
            name,
            last: Arc::new(Mutex::new(AnalogVoltage(0))),
            analog,
        })
    }

    #[must_use]
    pub fn out_port(&self) -> &OutPort<AnalogVoltage> {
        &self.analog
    }

    /// Convert `level` and drive the analog output.
    pub fn drive(&self, level: DigitalLevel) -> AnalogVoltage {
        let v = self.convert(level);
        self.analog.on_input(&[v], 0);
        v
    }

    /// Wire sink: convert digital samples, then call `analog_sink` (e.g. a probe tap).
    #[must_use]
    pub fn and_then(
        &self,
        analog_sink: impl Fn(&[AnalogVoltage], usize) + Send + Sync + 'static,
    ) -> impl Fn(&[DigitalLevel], usize) + Send + Sync + 'static {
        let last = Arc::clone(&self.last);
        let analog = self.analog.clone();
        move |values, changed| {
            if let Some(&level) = values.get(changed) {
                let v = convert(level, &last);
                analog.on_input(&[v], 0);
                analog_sink(&[v], 0);
            }
        }
    }

    fn convert(&self, level: DigitalLevel) -> AnalogVoltage {
        convert(level, &self.last)
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
        *self.last.lock().expect("dac last") = AnalogVoltage(0);
        self.analog.on_input(&[AnalogVoltage(0)], 0);
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
    use sim_kernel::MemoryMapBuilder;

    fn empty_bus() -> MemoryBus {
        MemoryMapBuilder::new().build().unwrap()
    }

    #[test]
    fn high_is_five_volts() {
        let dac = LevelDac::new("dac0").unwrap();
        assert_eq!(dac.drive(DigitalLevel::High), AnalogVoltage(HIGH_UV));
        assert_eq!(
            dac.out_port().pending().last().copied(),
            Some(AnalogVoltage(HIGH_UV))
        );
    }

    #[test]
    fn low_is_zero_volts() {
        let dac = LevelDac::new("dac0").unwrap();
        dac.drive(DigitalLevel::High);
        dac.out_port().clear_pending();
        assert_eq!(dac.drive(DigitalLevel::Low), AnalogVoltage(0));
        assert_eq!(
            dac.out_port().pending().last().copied(),
            Some(AnalogVoltage(0))
        );
    }

    #[test]
    fn hiz_holds_last_voltage() {
        let dac = LevelDac::new("dac0").unwrap();
        assert_eq!(dac.drive(DigitalLevel::HiZ), AnalogVoltage(0));
        dac.drive(DigitalLevel::High);
        dac.out_port().clear_pending();
        assert_eq!(dac.drive(DigitalLevel::HiZ), AnalogVoltage(HIGH_UV));
        assert_eq!(
            dac.out_port().pending().last().copied(),
            Some(AnalogVoltage(HIGH_UV))
        );
    }

    #[test]
    fn reset_clears_held_voltage() {
        let mut dac = LevelDac::new("dac0").unwrap();
        dac.drive(DigitalLevel::High);
        let mut bus = empty_bus();
        dac.reset(&mut bus);
        assert_eq!(
            dac.out_port().pending().last().copied(),
            Some(AnalogVoltage(0))
        );
        assert_eq!(dac.drive(DigitalLevel::HiZ), AnalogVoltage(0));
    }
}
