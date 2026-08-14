//! G23 on-chip SFR fabric: clock + SAU0 + TAU0 behind two MMIO windows.

use std::sync::{Arc, Mutex};

use sim_kernel::{Addr, BusError, EventCtx, MemoryMapped, ScheduledWork, SimEvent, Tick};

use crate::map::{ESFR_BASE, ESFR_SIZE, SFR_BASE, SFR_SIZE};
use crate::peripherals::clock::ClockGenerator;
use crate::peripherals::sau::{CHANNELS as SAU_CH, SauUnit};
use crate::peripherals::tau::{CHANNELS as TAU_CH, TauUnit};

/// Shared on-chip state for the R7F100GxL-class map.
pub struct G23Peripherals {
    pub clock: ClockGenerator,
    pub sau: SauUnit,
    pub tau: TauUnit,
    pending: Vec<ScheduledWork>,
    tau_started: [bool; TAU_CH],
}

impl Default for G23Peripherals {
    fn default() -> Self {
        Self::from_option_byte(None)
    }
}

impl G23Peripherals {
    #[must_use]
    pub fn from_option_byte(option_byte: Option<u8>) -> Self {
        Self {
            clock: ClockGenerator::from_option_byte(option_byte),
            sau: SauUnit::default(),
            tau: TauUnit::default(),
            pending: Vec::new(),
            tau_started: [false; TAU_CH],
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

    fn collect_scheduled(
        &mut self,
        now: Tick,
        inner: &Arc<Mutex<G23Peripherals>>,
        out: &mut Vec<ScheduledWork>,
    ) {
        out.append(&mut self.pending);
        let fclk = self.clock.f_clk_hz();
        for ch in 0..TAU_CH {
            let on = self.tau.channel_enabled(ch);
            if on && !self.tau_started[ch] {
                self.tau_started[ch] = true;
                if let Some(ns) = self.tau.interval_ns(ch, fclk) {
                    out.push(ScheduledWork {
                        at: now.saturating_add(Tick(ns)),
                        event: Box::new(TauExpire {
                            peri: Arc::clone(inner),
                            channel: ch as u8,
                        }),
                    });
                }
            }
            if !on {
                self.tau_started[ch] = false;
            }
        }
        for ch in 0..SAU_CH {
            if self.sau.take_want_tx(ch) {
                if let Some(ns) = self.sau.frame_ns(ch, fclk) {
                    self.sau.begin_tx(ch);
                    out.push(ScheduledWork {
                        at: now.saturating_add(Tick(ns)),
                        event: Box::new(SauTxDone {
                            peri: Arc::clone(inner),
                            channel: ch as u8,
                        }),
                    });
                }
            }
        }
    }
}

struct TauExpire {
    peri: Arc<Mutex<G23Peripherals>>,
    channel: u8,
}

impl SimEvent for TauExpire {
    fn fire(&mut self, ctx: &mut EventCtx<'_>) {
        let mut g = self.peri.lock().expect("g23 peripherals");
        let ch = self.channel as usize;
        if !g.tau.channel_enabled(ch) {
            g.tau_started[ch] = false;
            return;
        }
        g.tau.on_interval_expire(ch);
        if let Some(ns) = g.tau.interval_ns(ch, g.clock.f_clk_hz()) {
            g.pending.push(ScheduledWork {
                at: ctx.now.saturating_add(Tick(ns)),
                event: Box::new(TauExpire {
                    peri: Arc::clone(&self.peri),
                    channel: self.channel,
                }),
            });
        }
    }
}

struct SauTxDone {
    peri: Arc<Mutex<G23Peripherals>>,
    channel: u8,
}

impl SimEvent for SauTxDone {
    fn fire(&mut self, _ctx: &mut EventCtx<'_>) {
        let mut g = self.peri.lock().expect("g23 peripherals");
        g.sau.complete_tx(self.channel as usize);
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

    fn collect_scheduled(&mut self, now: Tick, out: &mut Vec<ScheduledWork>) {
        let mut guard = self.inner.lock().expect("g23 peripherals");
        guard.collect_scheduled(now, &self.inner, out);
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
