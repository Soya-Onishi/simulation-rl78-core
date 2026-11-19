//! Unitless scalar payload and [`DataPlane`] impl.

use iceoryx2_bb_derive_macros::ZeroCopySend;
use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;

use super::DataPlane;

/// Unitless / domain-defined `f64` sample on a board edge (e.g. ℃ or L/min).
///
/// The numeric meaning is fixed by the topology port name and plant docs, not by
/// this type. Layout is shared with cluster IPC (`repr(C)` + [`ZeroCopySend`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd, ZeroCopySend)]
pub struct Scalar(pub f64);

impl DataPlane for Scalar {
    const DEFAULT_RING_CAPACITY: usize = 256;
}
