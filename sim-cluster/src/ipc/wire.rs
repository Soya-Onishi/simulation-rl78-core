//! Zero-copy wire payloads for UART (control messages live in [`crate::control`]).

use iceoryx2::prelude::ZeroCopySend;

/// Packed UART frame (host endian).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ZeroCopySend)]
pub struct UartFrame {
    pub data: u16,
    pub data_bits: u8,
    pub stop_bits: u8,
    pub parity: u8,
    pub inverted: u8,
    pub _pad: u16,
    pub bit_time_ns: u64,
}
