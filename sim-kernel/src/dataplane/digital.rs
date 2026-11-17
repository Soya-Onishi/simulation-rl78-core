//! Digital level payload and [`DataPlane`] impl.

use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;

use super::DataPlane;

/// Digital level used as a wire / IPC payload.
///
/// Layout is shared with cluster probe IPC (`repr(u8)` + [`ZeroCopySend`]).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DigitalLevel {
    #[default]
    HiZ = 0,
    High = 1,
    Low = 2,
}

// `#[repr(u8)]` cannot combine with the ZeroCopySend derive's `#[repr(C)]`.
unsafe impl ZeroCopySend for DigitalLevel {}

impl DataPlane for DigitalLevel {
    const DEFAULT_RING_CAPACITY: usize = 256;
}
