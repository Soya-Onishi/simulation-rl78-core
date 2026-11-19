//! UART frame payload and [`DataPlane`] impl.

use iceoryx2_bb_derive_macros::ZeroCopySend;
use iceoryx2_bb_elementary_traits::zero_copy_send::ZeroCopySend;

use super::DataPlane;
use crate::Tick;

/// UART stop-bit count carried on a [`UartFrame`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UartStopBits {
    #[default]
    One = 0,
    Two = 1,
}

unsafe impl ZeroCopySend for UartStopBits {}

/// UART parity carried on a [`UartFrame`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UartParity {
    #[default]
    None = 0,
    Even = 1,
    Odd = 2,
}

unsafe impl ZeroCopySend for UartParity {}

/// One UART character plus link parameters, used as a wire / IPC payload.
///
/// Serial lines are not modeled as digital bit streams. `bit_time` is the
/// duration of one bit on the virtual clock (not a raw baud integer).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, ZeroCopySend)]
pub struct UartFrame {
    pub data: u16,
    pub data_bits: u8,
    pub bit_time: Tick,
    pub stop_bits: UartStopBits,
    pub parity: UartParity,
    pub inverted: bool,
}

impl Default for UartFrame {
    fn default() -> Self {
        Self {
            data: 0,
            data_bits: 8,
            bit_time: Tick::ZERO,
            stop_bits: UartStopBits::One,
            parity: UartParity::None,
            inverted: false,
        }
    }
}

impl DataPlane for UartFrame {
    const DEFAULT_RING_CAPACITY: usize = 256;
}
