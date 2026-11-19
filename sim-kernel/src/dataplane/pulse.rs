//! Pulse-train payload and [`DataPlane`] impl.

use iceoryx2_bb_derive_macros::ZeroCopySend;
use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;

use super::DataPlane;

/// Repeating digital pulse train parameters (hi/lo widths in virtual nanoseconds).
///
/// Used as a board-edge / cluster IPC payload so producers send waveform settings
/// instead of toggling [`crate::DigitalLevel`] on every edge. A receiver expands the
/// train onto a GPIO [`crate::SourcePort`]. If **either** width is `0`, the train is
/// stopped and the expanded line stays [`crate::DigitalLevel::Low`] ([`Pulse::STOPPED`]
/// sets both to `0`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, ZeroCopySend)]
pub struct Pulse {
    pub hi_width_ns: u64,
    pub lo_width_ns: u64,
}

impl Pulse {
    /// Stopped train (receiver holds the line low).
    pub const STOPPED: Self = Self {
        hi_width_ns: 0,
        lo_width_ns: 0,
    };

    /// True when the train should not toggle (line held low).
    #[must_use]
    pub const fn is_stopped(self) -> bool {
        self.hi_width_ns == 0 || self.lo_width_ns == 0
    }
}

impl DataPlane for Pulse {
    const DEFAULT_RING_CAPACITY: usize = 64;
}
