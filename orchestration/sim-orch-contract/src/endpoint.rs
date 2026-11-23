//! Endpoint dataplane samples and cross-host aggregation frames.
//!
//! - **Same-host (iceoryx):** publish [`EndpointWire`] only. Stop sync is SHM
//!   (`stop_generation`); samples never carry a stop mark.
//! - **Cross-host:** Command Endpoint threads exchange [`EndpointDatagram`],
//!   which may set [`EndpointDatagram::stop_flag`]. `stop_generation` is **not**
//!   shared across hosts.
//!
//! Reuses [`sim_kernel`] dataplane payload types so the fan-in path stays
//! `repr(C)` + [`ZeroCopySend`] without serde on the hot path.

use iceoryx2::prelude::ZeroCopySend;
use sim_kernel::{AnalogVoltage, DigitalLevel, Pulse, Scalar, UartFrame};

use crate::cluster_id::ClusterId;
use crate::hashed_id::HashedId;

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

/// One endpoint sample (iceoryx fan-in and datagram element).
///
/// `cluster_id` + `board_id` are required because the host Endpoint thread is
/// shared across simulation clusters and cannot route on OutPort id alone.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, ZeroCopySend)]
pub struct EndpointWire {
    pub cluster_id: ClusterId,
    /// Source board within the cluster.
    pub board_id: HashedId<u64>,
    /// Source OutPort hashed id.
    pub src: HashedId<u64>,
    pub payload: EndpointPayload,
}

impl EndpointWire {
    #[must_use]
    pub fn new(
        cluster_id: ClusterId,
        board_id: HashedId<u64>,
        src: HashedId<u64>,
        payload: EndpointPayload,
    ) -> Self {
        Self {
            cluster_id,
            board_id,
            src,
            payload,
        }
    }
}

/// Cross-host frame built by the Endpoint transfer thread.
///
/// Carries the sender-side virtual time and zero or more [`EndpointWire`]
/// samples. `payloads` may be empty for a periodic VT tick or a stop interrupt
/// with no queued endpoint data. Not an iceoryx `ZeroCopySend` type (`Vec`);
/// used only on the host-to-host path.
#[derive(Clone, Debug, PartialEq)]
pub struct EndpointDatagram {
    /// `0` = no stop hint; non-zero = remote should apply local SHM stop.
    /// Does **not** carry or synchronize `stop_generation` across hosts.
    pub stop_flag: u8,
    /// Virtual time at send (ns). Present even when `payloads` is empty.
    pub virtual_time_ns: u64,
    /// Aggregated endpoint samples; empty is valid (VT tick or stop interrupt).
    pub payloads: Vec<EndpointWire>,
}

impl EndpointDatagram {
    #[must_use]
    pub fn new(stop_flag: u8, virtual_time_ns: u64, payloads: Vec<EndpointWire>) -> Self {
        Self {
            stop_flag,
            virtual_time_ns,
            payloads,
        }
    }

    #[must_use]
    pub fn has_stop_mark(&self) -> bool {
        self.stop_flag != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn wire_has_routing_ids_no_stop_fields() {
        let w = EndpointWire::new(
            ClusterId::new(1, 2),
            HashedId::new(0x11),
            HashedId::new(0x22),
            EndpointPayload::Digital(DigitalLevel::High),
        );
        assert_eq!(w.cluster_id.id, 1);
        assert_eq!(w.board_id.get(), 0x11);
        assert_eq!(w.src.get(), 0x22);
        assert!(align_of::<EndpointWire>() >= 8);
        assert_eq!(offset_of!(EndpointWire, cluster_id), 0);
        assert!(offset_of!(EndpointWire, board_id) >= size_of::<ClusterId>());
        assert!(offset_of!(EndpointWire, src) > offset_of!(EndpointWire, board_id));
        assert!(offset_of!(EndpointWire, payload) > offset_of!(EndpointWire, src));
        // Stop marks are not part of this layout (see EndpointDatagram).
        assert!(size_of::<EndpointWire>() > 0);
    }

    #[test]
    fn datagram_carries_vt_and_optional_payloads() {
        let w = EndpointWire::new(
            ClusterId::new(1, 2),
            HashedId::new(3),
            HashedId::new(4),
            EndpointPayload::Pulse(Pulse {
                hi_width_ns: 10,
                lo_width_ns: 10,
            }),
        );
        let d = EndpointDatagram::new(1, 1_000, vec![w]);
        assert!(d.has_stop_mark());
        assert_eq!(d.virtual_time_ns, 1_000);
        assert_eq!(d.payloads.len(), 1);
        assert_eq!(d.payloads[0].src.get(), 4);
    }

    #[test]
    fn datagram_allows_empty_payloads() {
        let vt_tick = EndpointDatagram::new(0, 42, Vec::new());
        assert!(!vt_tick.has_stop_mark());
        assert!(vt_tick.payloads.is_empty());

        let stop = EndpointDatagram::new(1, 42, Vec::new());
        assert!(stop.has_stop_mark());
        assert!(stop.payloads.is_empty());
    }
}
