//! Device-dependent physical memory layout.
//!
//! RL78 flash / RAM windows differ by core / part number. Callers pick an
//! [`Rl78Device`] (or supply a custom [`MemoryLayout`]) instead of hard-coding
//! one map for every chip.

use sim_kernel::Addr;

/// Code / data windows for one RL78 core configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryLayout {
    pub rom_base: Addr,
    pub rom_size: usize,
    pub ram_base: Addr,
    pub ram_size: usize,
}

/// Known RL78 devices. Add variants as concrete R5F parts are supported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rl78Device {
    /// Milestone-1 stand-in (64 KiB flash, 32 KiB RAM) until a real part is chosen.
    #[default]
    Generic64k,
    /// RL78/G23 part. Peripheral map is [`crate::peripherals::Rl78G23Core`];
    /// this variant only supplies flash/RAM windows.
    R7F100Gxl,
}

impl Rl78Device {
    #[must_use]
    pub fn memory_layout(self) -> MemoryLayout {
        match self {
            Self::Generic64k => MemoryLayout {
                rom_base: 0x00000,
                rom_size: 64 * 1024,
                // Leave 0xF0000–0xF00FF for [`MAGIC_PROBE_BASE`] (ABS16 mirror).
                ram_base: 0xF0100,
                ram_size: 32 * 1024,
            },
            // QEMU `hw/rl78/r7f100gxl.h`: flash 128 KiB, RAM at 0xF3F00 size 0xC000.
            Self::R7F100Gxl => MemoryLayout {
                rom_base: 0x00000,
                rom_size: 0x20000,
                ram_base: 0xF3F00,
                ram_size: 0xC000,
            },
        }
    }
}

/// Dummy logger peripheral. Guest stores here become host stdout.
///
/// Placed at `0xF0000` so RL78 `MOV !addr16, #imm` (phys = `addr16 | 0xF0000`)
/// can reach it without an ES prefix — same idea as tlib's harness MMIO page.
pub const MAGIC_PROBE_BASE: Addr = 0xF0000;
pub const MAGIC_PROBE_SIZE: usize = 256;
