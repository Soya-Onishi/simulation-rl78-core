//! Analog voltage payload and [`DataPlane`] impl.

use iceoryx2_bb_derive_macros::ZeroCopySend;
use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;

use super::DataPlane;

/// Analog voltage in microvolts, used as a wire / IPC payload.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, ZeroCopySend)]
pub struct AnalogVoltage(pub i64);

impl DataPlane for AnalogVoltage {
    const DEFAULT_RING_CAPACITY: usize = 256;
}
