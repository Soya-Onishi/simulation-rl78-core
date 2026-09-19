//! Device reset (QEMU `Resettable` hold phase).

use crate::bus::MemoryBus;

/// Hardware reset. Construction (`new`) must not apply this; call after wiring.
///
/// `bus` is available for ROM-backed inputs (e.g. option bytes). Implementations
/// must not re-enter their own MMIO via `bus` while holding interior locks.
pub trait Resettable {
    fn reset(&mut self, bus: &mut MemoryBus);
}

/// Logical device owned by [`crate::Machine`] (SoC part, board peripheral, …).
///
/// Reset is driven through this handle — not by walking [`crate::MemoryMapped`]
/// regions on the bus.
pub trait Device: Send + Resettable {}
