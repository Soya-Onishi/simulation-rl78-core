//! Same-host endpoint dataplane wire: one tagged sample on one subscriber.
//!
//! Reuses [`sim_kernel`] dataplane payload types (`UartFrame`, `AnalogVoltage`,
//! `DigitalLevel`, `Pulse`, `Scalar`) so the fan-in path stays `repr(C)` +
//! [`ZeroCopySend`] without serde on the hot path.
//!
//! Cross-host transfers may set [`EndpointWire::stop_flag`]; same-host iceoryx
//! samples must leave it clear and rely on SHM stop generation instead.

use iceoryx2::prelude::ZeroCopySend;
use sim_kernel::{AnalogVoltage, DigitalLevel, Pulse, Scalar, UartFrame};

/// Unified payload kinds carried on the per-node endpoint subscriber.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ZeroCopySend)]
pub enum EndpointPayload {
    Uart(UartFrame),
    Analog(AnalogVoltage),
    Digital(DigitalLevel),
    Pulse(Pulse),
    Scalar(Scalar),
}

/// One endpoint sample published to a node's fan-in subscriber.
///
/// Layout: `{ src, stop_flag, stop_generation, payload }`. `stop_flag == 0`
/// means no cross-host stop mark; non-zero means `stop_generation` is meaningful.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ZeroCopySend)]
pub struct EndpointWire {
    /// Hashed id of the source OutPort.
    pub src: u64,
    /// `0` = no stop mark (required on same-host); non-zero = cross-host stop.
    pub stop_flag: u8,
    pub _pad: [u8; 7],
    /// Stop generation when `stop_flag != 0`; otherwise ignored (prefer `0`).
    pub stop_generation: u64,
    pub payload: EndpointPayload,
}

impl EndpointWire {
    /// Same-host constructor (no stop mark).
    #[must_use]
    pub fn same_host(src: u64, payload: EndpointPayload) -> Self {
        Self {
            src,
            stop_flag: 0,
            _pad: [0; 7],
            stop_generation: 0,
            payload,
        }
    }

    /// Cross-host constructor with stop mark.
    #[must_use]
    pub fn cross_host_stop(src: u64, stop_generation: u64, payload: EndpointPayload) -> Self {
        Self {
            src,
            stop_flag: 1,
            _pad: [0; 7],
            stop_generation,
            payload,
        }
    }

    /// True when a cross-host stop mark is present.
    #[must_use]
    pub fn has_stop_mark(self) -> bool {
        self.stop_flag != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, size_of};

    #[test]
    fn same_host_has_no_stop() {
        let w = EndpointWire::same_host(1, EndpointPayload::Digital(DigitalLevel::High));
        assert!(!w.has_stop_mark());
        assert_eq!(w.src, 1);
    }

    #[test]
    fn wire_is_copy_aligned() {
        assert!(align_of::<EndpointWire>() >= 8);
        assert!(size_of::<EndpointWire>() > 0);
        let _ = EndpointWire::cross_host_stop(
            9,
            3,
            EndpointPayload::Pulse(Pulse {
                hi_width_ns: 10,
                lo_width_ns: 10,
            }),
        );
    }
}
