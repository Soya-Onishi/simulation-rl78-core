//! G23 on-chip SFR fabric: clock + SAU0 + TAU0 behind two MMIO windows.

use std::sync::{Arc, Mutex};

use sim_kernel::{Addr, BusError, MemoryMapped};

use crate::map::{ESFR_BASE, ESFR_SIZE, SFR_BASE, SFR_SIZE};
use crate::peripherals::clock::ClockGenerator;
use crate::peripherals::sau::SauUnit;
use crate::peripherals::tau::TauUnit;

/// Shared on-chip state for the R7F100GxL-class map.
#[derive(Clone, Debug, Default)]
pub struct G23Peripherals {
    pub clock: ClockGenerator,
    pub sau: SauUnit,
    pub tau: TauUnit,
}

impl G23Peripherals {
    #[must_use]
    pub fn from_option_byte(option_byte: Option<u8>) -> Self {
        Self {
            clock: ClockGenerator::from_option_byte(option_byte),
            sau: SauUnit::default(),
            tau: TauUnit::default(),
        }
    }

    fn read_u8(&self, addr: u32) -> u8 {
        if ClockGenerator::owns(addr) {
            self.clock.read_u8(addr)
        } else if SauUnit::owns(addr) {
            self.sau.read_u8(addr)
        } else if TauUnit::owns(addr) {
            self.tau.read_u8(addr)
        } else {
            0
        }
    }

    fn write_u8(&mut self, addr: u32, value: u8) {
        if ClockGenerator::owns(addr) {
            self.clock.write_u8(addr, value);
        } else if SauUnit::owns(addr) {
            self.sau.write_u8(addr, value);
        } else if TauUnit::owns(addr) {
            self.tau.write_u8(addr, value);
        }
    }
}

/// One mapped window (SFR or ESFR) over shared [`G23Peripherals`].
pub struct SfrWindow {
    inner: Arc<Mutex<G23Peripherals>>,
    base: Addr,
    len: u64,
}

impl SfrWindow {
    #[must_use]
    pub fn sfr(inner: Arc<Mutex<G23Peripherals>>) -> Self {
        Self {
            inner,
            base: SFR_BASE,
            len: SFR_SIZE as u64,
        }
    }

    #[must_use]
    pub fn esfr(inner: Arc<Mutex<G23Peripherals>>) -> Self {
        Self {
            inner,
            base: ESFR_BASE,
            len: ESFR_SIZE as u64,
        }
    }
}

impl MemoryMapped for SfrWindow {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        self.check(offset, buf.len())?;
        let guard = self.inner.lock().expect("g23 peripherals");
        for (i, slot) in buf.iter_mut().enumerate() {
            let addr = (self.base + offset + i as u64) as u32;
            *slot = guard.read_u8(addr);
        }
        Ok(())
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        self.check(offset, buf.len())?;
        let mut guard = self.inner.lock().expect("g23 peripherals");
        for (i, value) in buf.iter().enumerate() {
            let addr = (self.base + offset + i as u64) as u32;
            guard.write_u8(addr, *value);
        }
        Ok(())
    }

    fn host_ptr(&mut self) -> Option<*mut u8> {
        None
    }

    fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::NotLoadable {
            addr: self.base + offset,
        })
    }
}

impl SfrWindow {
    fn check(&self, offset: u64, len: usize) -> Result<(), BusError> {
        if offset.saturating_add(len as u64) > self.len {
            Err(BusError::OutOfRange {
                addr: self.base + offset,
                offset,
                len,
            })
        } else {
            Ok(())
        }
    }
}

/// Allocate SFR + ESFR windows that share one peripheral block.
#[must_use]
pub fn g23_sfr_windows(peri: Arc<Mutex<G23Peripherals>>) -> (SfrWindow, SfrWindow) {
    (SfrWindow::sfr(Arc::clone(&peri)), SfrWindow::esfr(peri))
}
