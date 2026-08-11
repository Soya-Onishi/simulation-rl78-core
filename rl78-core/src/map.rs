//! Simplified RL78-like physical map used until a real MCU description exists.

/// Code flash window (64 KiB).
pub const ROM_BASE: u64 = 0x00000;
pub const ROM_SIZE: usize = 64 * 1024;

/// Near RAM window (32 KiB) placed below the SFR / Magic probe area.
pub const RAM_BASE: u64 = 0xF0000;
pub const RAM_SIZE: usize = 32 * 1024;

/// Dummy logger peripheral. Guest stores here become host stdout.
pub const MAGIC_PROBE_BASE: u64 = 0xFFF00;
pub const MAGIC_PROBE_SIZE: usize = 16;
