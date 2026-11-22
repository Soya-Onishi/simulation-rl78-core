//! Host control shared-memory layout (VT ceiling, board VT, stop generation).
//!
//! Replaces the legacy `vt_clock` file (ceiling + board VT only). Layout is
//! versioned and `repr(C)` with explicit padding. Mapping (mmap) lives in
//! runtime crates; this module is the bytes-in-memory contract on little-endian
//! hosts (the only supported target class for this project).
//!
//! Same-host stop sync uses this SHM only. Endpoint iceoryx payloads on the
//! same host must not carry a stop mark (#141).

#[cfg(not(target_endian = "little"))]
compile_error!("sim-orch-contract host_ctrl requires little-endian targets");

use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};

/// File name under the cluster iceoryx root (replaces `vt_clock`).
pub const HOST_CTRL_FILE: &str = "host_ctrl";

/// Layout format version stored in [`HostCtrlHeader::version`].
pub const HOST_CTRL_VERSION: u32 = 1;

/// Magic: ASCII `SCTL` (simulation control), stored as a native `u32` whose
/// little-endian bytes are `SCTL`.
pub const HOST_CTRL_MAGIC: u32 = u32::from_le_bytes(*b"SCTL");

/// Stop generation value that must not advance further (saturation).
///
/// Writers treat this as sticky stop until the mapping is recreated for a new
/// session.
pub const STOP_GEN_SATURATED: u64 = u64::MAX;

/// Fixed header at offset 0 of `{iox_root}/host_ctrl`.
///
/// After the header, `board_count` native `u64` board virtual-time slots follow
/// at [`HostCtrlLayout::BOARDS_OFFSET`] (8-byte aligned). Concurrent readers and
/// writers must access `ceiling_ns`, `stop_generation`, and board VT slots via
/// [`AtomicU64`] (see [`bump_stop_generation`]); do not tear with plain loads
/// across processes.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug)]
pub struct HostCtrlHeader {
    /// [`HOST_CTRL_MAGIC`].
    pub magic: u32,
    /// [`HOST_CTRL_VERSION`].
    pub version: u32,
    /// Topology board count `N` (`N >= 1`).
    pub board_count: u32,
    /// Padding to 8-byte alignment before `u64` fields (not part of the API).
    pad0: u32,
    /// Arbiter hard ceiling in nanoseconds (`u64::MAX` = unlimited).
    pub ceiling_ns: u64,
    /// Cluster-wide stop generation.
    pub stop_generation: u64,
}

/// Helpers for sizing and indexing the mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostCtrlLayout {
    pub board_count: u32,
}

impl HostCtrlLayout {
    /// Byte offset where board VT slot 0 begins.
    pub const BOARDS_OFFSET: usize = size_of::<HostCtrlHeader>();

    /// Create a layout for `board_count` boards (`>= 1`).
    #[must_use]
    pub fn new(board_count: u32) -> Option<Self> {
        if board_count == 0 {
            return None;
        }
        Some(Self { board_count })
    }

    /// Byte offset of board VT for topology board index `board_index` (`0..N`).
    #[must_use]
    pub fn board_vt_byte_offset(self, board_index: u32) -> Option<usize> {
        if board_index >= self.board_count {
            return None;
        }
        Some(Self::BOARDS_OFFSET + board_index as usize * size_of::<u64>())
    }

    /// Total mapping size in bytes.
    #[must_use]
    pub fn total_bytes(self) -> usize {
        Self::BOARDS_OFFSET + self.board_count as usize * size_of::<u64>()
    }

    /// Initial header for a newly created mapping.
    #[must_use]
    pub fn fresh_header(self) -> HostCtrlHeader {
        HostCtrlHeader {
            magic: HOST_CTRL_MAGIC,
            version: HOST_CTRL_VERSION,
            board_count: self.board_count,
            pad0: 0,
            ceiling_ns: u64::MAX,
            stop_generation: 0,
        }
    }
}

/// Pure saturating bump for stop generation (single-threaded / after a load).
#[must_use]
pub const fn saturating_bump_stop_gen(current: u64) -> u64 {
    match current.checked_add(1) {
        Some(n) => n,
        None => STOP_GEN_SATURATED,
    }
}

/// Atomically bump `stop_generation`, saturating at [`STOP_GEN_SATURATED`].
///
/// Returns the value stored after the bump.
pub fn bump_stop_generation(slot: &AtomicU64) -> u64 {
    let mut prev = slot.load(Ordering::Acquire);
    loop {
        let next = saturating_bump_stop_gen(prev);
        match slot.compare_exchange_weak(prev, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(observed) => prev = observed,
        }
    }
}

/// Atomic view of the concurrently updated `u64` slots in a mapped `host_ctrl`.
///
/// Plain [`HostCtrlHeader`] is for create/init and layout docs. Runtime code
/// that shares the mapping across processes must use this view (or equivalent
/// `AtomicU64` accesses at the documented offsets).
pub struct HostCtrlAtoms<'a> {
    pub ceiling_ns: &'a AtomicU64,
    pub stop_generation: &'a AtomicU64,
    pub board_vt: &'a [AtomicU64],
}

impl<'a> HostCtrlAtoms<'a> {
    /// Build an atomic view over an existing mapping.
    ///
    /// # Safety
    /// - `base` must point to a mapping of at least [`HostCtrlLayout::total_bytes`].
    /// - `base` must be 8-byte aligned.
    /// - The header at `base` must already contain a matching `board_count`.
    /// - No concurrent non-atomic writers to these slots.
    #[must_use]
    pub unsafe fn from_mapping(base: *mut u8, layout: HostCtrlLayout) -> Self {
        debug_assert_eq!(base.align_offset(8), 0);
        // SAFETY: caller guarantees alignment, size, and exclusive atomic protocol.
        let ceiling = unsafe { &*base.add(16).cast::<AtomicU64>() };
        let stop = unsafe { &*base.add(24).cast::<AtomicU64>() };
        let boards = unsafe {
            std::slice::from_raw_parts(
                base.add(HostCtrlLayout::BOARDS_OFFSET).cast::<AtomicU64>(),
                layout.board_count as usize,
            )
        };
        Self {
            ceiling_ns: ceiling,
            stop_generation: stop,
            board_vt: boards,
        }
    }

    /// Saturating bump of stop generation.
    pub fn bump_stop(&self) -> u64 {
        bump_stop_generation(self.stop_generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::align_of;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn header_alignment_and_offsets() {
        assert_eq!(align_of::<HostCtrlHeader>(), 8);
        assert_eq!(size_of::<HostCtrlHeader>(), 32);
        assert_eq!(std::mem::offset_of!(HostCtrlHeader, magic), 0);
        assert_eq!(std::mem::offset_of!(HostCtrlHeader, version), 4);
        assert_eq!(std::mem::offset_of!(HostCtrlHeader, board_count), 8);
        assert_eq!(std::mem::offset_of!(HostCtrlHeader, pad0), 12);
        assert_eq!(std::mem::offset_of!(HostCtrlHeader, ceiling_ns), 16);
        assert_eq!(std::mem::offset_of!(HostCtrlHeader, stop_generation), 24);
        assert_eq!(HostCtrlLayout::BOARDS_OFFSET % 8, 0);
    }

    #[test]
    fn layout_sizing() {
        let layout = HostCtrlLayout::new(3).unwrap();
        assert_eq!(layout.total_bytes(), 32 + 3 * 8);
        assert_eq!(layout.board_vt_byte_offset(0), Some(32));
        assert_eq!(layout.board_vt_byte_offset(2), Some(48));
        assert_eq!(layout.board_vt_byte_offset(3), None);
        assert!(HostCtrlLayout::new(0).is_none());
    }

    #[test]
    fn stop_gen_saturates() {
        assert_eq!(saturating_bump_stop_gen(0), 1);
        assert_eq!(
            saturating_bump_stop_gen(STOP_GEN_SATURATED - 1),
            STOP_GEN_SATURATED
        );
        assert_eq!(
            saturating_bump_stop_gen(STOP_GEN_SATURATED),
            STOP_GEN_SATURATED
        );
    }

    #[test]
    fn bump_stop_generation_atomic() {
        let slot = AtomicU64::new(STOP_GEN_SATURATED - 1);
        assert_eq!(bump_stop_generation(&slot), STOP_GEN_SATURATED);
        assert_eq!(bump_stop_generation(&slot), STOP_GEN_SATURATED);
    }

    #[test]
    fn magic_is_sctl_le() {
        assert_eq!(&HOST_CTRL_MAGIC.to_le_bytes(), b"SCTL");
    }
}
