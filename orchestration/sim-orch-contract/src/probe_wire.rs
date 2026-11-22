//! Live probe iceoryx samples on `sim-cluster/{cluster_key}/probe` (#141).
//!
//! One tagged publisher replaces today's per-probe typed services. Disk JSON
//! spill uses [`crate::probe_log`]; this module is the ZeroCopy hot path only.

use iceoryx2::prelude::ZeroCopySend;
use sim_kernel::{AnalogVoltage, DigitalLevel, UartFrame};

/// One coarse / live probe sample published by the Node Probe thread.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ZeroCopySend)]
pub enum ProbeLiveSample {
    Digital {
        /// Probe id hash (`HashedId<u64>` numeric value).
        probe_id: u64,
        virtual_time_ns: u64,
        value: DigitalLevel,
    },
    Uart {
        probe_id: u64,
        virtual_time_ns: u64,
        value: UartFrame,
    },
    Analog {
        probe_id: u64,
        virtual_time_ns: u64,
        value: AnalogVoltage,
    },
}

impl ProbeLiveSample {
    #[must_use]
    pub fn probe_id(self) -> u64 {
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
    fn accessors() {
        let s = ProbeLiveSample::Digital {
            probe_id: 7,
            virtual_time_ns: 99,
            value: DigitalLevel::High,
        };
        assert_eq!(s.probe_id(), 7);
        assert_eq!(s.virtual_time_ns(), 99);
    }
}
