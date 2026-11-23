//! Live probe iceoryx samples on `sim-cluster/{cluster}/probe` (#141).
//!
//! One tagged publisher replaces today's per-probe typed services. Disk JSON
//! spill uses [`crate::probe::log`]; this module is the ZeroCopy hot path only.
//!
//! Digital live values carry the same fields as [`crate::probe::DigitalProbeData`]
//! (including PWM `ratio` / `length`) so ProbeTap → mpsc → live/log stay aligned.

use iceoryx2::prelude::ZeroCopySend;
use sim_kernel::{AnalogVoltage, DigitalLevel, UartFrame};

use crate::id::HashedId;
use crate::probe::log::DigitalProbeData;

/// Digital probe value on the live / mpsc path (#141 `data` shape).
///
/// `length_ns == 0` means a constant level (JSON `length` omitted). PWM samples
/// set a non-zero period. `ratio` is Hi duty 0..=100 (constant Hi=100, Lo=0).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ZeroCopySend)]
pub struct DigitalLiveValue {
    /// Reference voltage.
    pub v: f64,
    /// Non-zero when high-impedance.
    pub hiz: u8,
    /// Hi duty ratio (0..=100).
    pub ratio: u8,
    _pad: [u8; 6],
    /// PWM period in ns; `0` = constant (no period).
    pub length_ns: u64,
}

impl DigitalLiveValue {
    /// Constant level (no PWM period), matching #141 constant rules.
    #[must_use]
    pub const fn constant(v: f64, level: DigitalLevel) -> Self {
        match level {
            DigitalLevel::HiZ => Self {
                v,
                hiz: 1,
                ratio: 0,
                _pad: [0; 6],
                length_ns: 0,
            },
            DigitalLevel::High => Self {
                v,
                hiz: 0,
                ratio: 100,
                _pad: [0; 6],
                length_ns: 0,
            },
            DigitalLevel::Low => Self {
                v,
                hiz: 0,
                ratio: 0,
                _pad: [0; 6],
                length_ns: 0,
            },
        }
    }

    /// PWM sample. Returns `None` when `length_ns == 0` or `ratio > 100`.
    #[must_use]
    pub const fn pwm(v: f64, ratio: u8, length_ns: u64) -> Option<Self> {
        if length_ns == 0 || ratio > 100 {
            return None;
        }
        Some(Self {
            v,
            hiz: 0,
            ratio,
            _pad: [0; 6],
            length_ns,
        })
    }

    #[must_use]
    pub const fn is_hiz(self) -> bool {
        self.hiz != 0
    }

    /// PWM period, or `None` when constant (`length_ns == 0`).
    #[must_use]
    pub const fn length(self) -> Option<u64> {
        if self.length_ns == 0 {
            None
        } else {
            Some(self.length_ns)
        }
    }
}

impl From<DigitalLiveValue> for DigitalProbeData {
    fn from(value: DigitalLiveValue) -> Self {
        Self {
            v: value.v,
            hiz: value.is_hiz(),
            ratio: value.ratio,
            length: value.length(),
        }
    }
}

impl From<DigitalProbeData> for DigitalLiveValue {
    fn from(value: DigitalProbeData) -> Self {
        Self {
            v: value.v,
            hiz: u8::from(value.hiz),
            ratio: value.ratio,
            _pad: [0; 6],
            length_ns: value.length.unwrap_or(0),
        }
    }
}

/// One coarse / live probe sample published by the Node Probe thread.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ZeroCopySend)]
pub enum ProbeLiveSample {
    Digital {
        /// Probe id.
        probe_id: HashedId<u64>,
        virtual_time_ns: u64,
        value: DigitalLiveValue,
    },
    Uart {
        probe_id: HashedId<u64>,
        virtual_time_ns: u64,
        value: UartFrame,
    },
    Analog {
        probe_id: HashedId<u64>,
        virtual_time_ns: u64,
        value: AnalogVoltage,
    },
}

impl ProbeLiveSample {
    #[must_use]
    pub fn probe_id(self) -> HashedId<u64> {
        match self {
            Self::Digital { probe_id, .. }
            | Self::Uart { probe_id, .. }
            | Self::Analog { probe_id, .. } => probe_id,
        }
    }

    #[must_use]
    pub fn virtual_time_ns(self) -> u64 {
        match self {
            Self::Digital {
                virtual_time_ns, ..
            }
            | Self::Uart {
                virtual_time_ns, ..
            }
            | Self::Analog {
                virtual_time_ns, ..
            } => virtual_time_ns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_constant_high() {
        let s = ProbeLiveSample::Digital {
            probe_id: HashedId::new(7),
            virtual_time_ns: 99,
            value: DigitalLiveValue::constant(3.3, DigitalLevel::High),
        };
        assert_eq!(s.probe_id(), HashedId::new(7));
        assert_eq!(s.virtual_time_ns(), 99);
        let ProbeLiveSample::Digital { value, .. } = s else {
            panic!("expected Digital");
        };
        assert_eq!(value.ratio, 100);
        assert_eq!(value.length(), None);
    }

    #[test]
    fn pwm_round_trip_to_log_data() {
        let live = DigitalLiveValue::pwm(5.0, 50, 1000).unwrap();
        let log = DigitalProbeData::from(live);
        assert_eq!(log.ratio, 50);
        assert_eq!(log.length, Some(1000));
        assert!(!log.hiz);
        let back = DigitalLiveValue::from(log);
        assert_eq!(back, live);
    }
}
