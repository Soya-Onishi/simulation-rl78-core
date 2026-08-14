//! Timer Array Unit 0 (QEMU `hw/rl78/tau.c`).

use std::sync::{Arc, Mutex};

use sim_kernel::{Addr, BusError, EventCtl, EventCtx, EventId, MemoryMapped, SimEvent, Tick};

use crate::peripherals::clock::ClockGenerator;

pub const CHANNELS: usize = 8;

/// TAU MMIO windows (QEMU `rl78g23_register_tau`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TauBank {
    /// `0xFFF18`, size 4: TDR0–1.
    Tdr01,
    /// `0xFFF64`, size 0xC: TDR2–7.
    Tdr27,
    /// `0xF0180`, size 0x40: TCR/TMR/TSR/TE/TS/TT/TPS/TO*.
    Ctrl,
    /// `0xF0074`, size 2: TIS0/TIS1.
    Tis,
}

impl TauBank {
    pub const ALL: [Self; 4] = [Self::Tdr01, Self::Tdr27, Self::Ctrl, Self::Tis];

    #[must_use]
    pub const fn base(self) -> Addr {
        match self {
            Self::Tdr01 => 0xFFF18,
            Self::Tdr27 => 0xFFF64,
            Self::Ctrl => 0xF0180,
            Self::Tis => 0xF0074,
        }
    }

    #[must_use]
    pub const fn size(self) -> u64 {
        match self {
            Self::Tdr01 => 4,
            Self::Tdr27 => 0x0C,
            Self::Ctrl => 0x40,
            Self::Tis => 2,
        }
    }
}

pub struct TauUnit {
    tdr: [u16; CHANNELS],
    tcr: [u16; CHANNELS],
    tmr: [u16; CHANNELS],
    tsr: [u16; CHANNELS],
    tps: u16,
    te: u16,
    to: u16,
    toe: u16,
    tol: u16,
    tom: u16,
    tis0: u8,
    tis1: u8,
    ctl: EventCtl,
    clock: Arc<Mutex<ClockGenerator>>,
    expire_id: [Option<EventId>; CHANNELS],
    generation: [u32; CHANNELS],
}

impl TauUnit {
    #[must_use]
    pub fn new(ctl: EventCtl, clock: Arc<Mutex<ClockGenerator>>) -> Self {
        Self {
            tdr: [0xFFFF; CHANNELS],
            tcr: [0xFFFF; CHANNELS],
            tmr: [0; CHANNELS],
            tsr: [0; CHANNELS],
            tps: 0,
            te: 0,
            to: 0,
            toe: 0,
            tol: 0,
            tom: 0,
            tis0: 0,
            tis1: 0,
            ctl,
            clock,
            expire_id: [None; CHANNELS],
            generation: [0; CHANNELS],
        }
    }

    #[must_use]
    pub fn channel_enabled(&self, channel: usize) -> bool {
        self.te & (1 << channel) != 0
    }

    fn fclk(&self) -> u32 {
        self.clock.lock().expect("clock").f_clk_hz()
    }

    fn interval_ns(&self, channel: usize) -> Option<u64> {
        let f_clk_hz = self.fclk();
        if f_clk_hz == 0 {
            return None;
        }
        let cks = ((self.tmr[channel] >> 14) & 0x3) as u8;
        let div = u64::from(self.ck_divider(cks));
        let counts = u64::from(self.tdr[channel]) + 1;
        Some(counts.saturating_mul(div).saturating_mul(1_000_000_000) / u64::from(f_clk_hz))
    }

    fn ck_divider(&self, cks: u8) -> u32 {
        let prs0 = self.tps & 0xF;
        let prs1 = (self.tps >> 4) & 0xF;
        let prs2 = (self.tps >> 8) & 0x3;
        let prs3 = (self.tps >> 12) & 0x3;
        match cks {
            0 => 1 << prs0,
            1 => 1 << prs1,
            2 => {
                if prs2 == 0 {
                    2
                } else {
                    1 << (prs2 * 2)
                }
            }
            _ => 1 << (8 + prs3 * 2),
        }
    }

    fn stop_channel(&mut self, ch: usize) {
        self.te &= !(1 << ch);
        self.generation[ch] = self.generation[ch].wrapping_add(1);
        if let Some(id) = self.expire_id[ch].take() {
            self.ctl.cancel(id);
        }
    }

    fn start_channel(&mut self, inner: &Arc<Mutex<TauUnit>>, ch: usize) {
        if self.expire_id[ch].is_some() {
            return;
        }
        let Some(ns) = self.interval_ns(ch) else {
            return;
        };
        self.te |= 1 << ch;
        let at = self.ctl.now().saturating_add(Tick(ns));
        let id = self.ctl.schedule(
            at,
            Box::new(TauExpire {
                inner: Arc::clone(inner),
                channel: ch as u8,
                generation: self.generation[ch],
                at,
            }),
        );
        self.expire_id[ch] = Some(id);
    }

    fn read16(&self, bank: TauBank, offset: u64) -> u16 {
        match bank {
            TauBank::Tdr01 => self.tdr[(offset / 2) as usize],
            TauBank::Tdr27 => self.tdr[2 + (offset / 2) as usize],
            TauBank::Tis => u16::from(self.tis0) | (u16::from(self.tis1) << 8),
            TauBank::Ctrl => {
                let ch = ((offset / 2) % CHANNELS as u64) as usize;
                match offset {
                    0x00..=0x0E => self.tcr[ch],
                    0x10..=0x1E => self.tmr[ch],
                    0x20..=0x2E => self.tsr[ch],
                    0x30 => self.te,
                    0x32 | 0x34 => 0,
                    0x36 => self.tps,
                    0x38 => self.to,
                    0x3A => self.toe,
                    0x3C => self.tol,
                    0x3E => self.tom,
                    _ => 0,
                }
            }
        }
    }

    fn write16(&mut self, bank: TauBank, offset: u64, value: u16, inner: &Arc<Mutex<TauUnit>>) {
        match bank {
            TauBank::Tdr01 => self.tdr[(offset / 2) as usize] = value,
            TauBank::Tdr27 => self.tdr[2 + (offset / 2) as usize] = value,
            TauBank::Tis => {
                self.tis0 = value as u8;
                self.tis1 = (value >> 8) as u8;
            }
            TauBank::Ctrl => {
                let ch = ((offset / 2) % CHANNELS as u64) as usize;
                match offset {
                    0x10..=0x1E => self.tmr[ch] = value,
                    0x32 => {
                        for i in 0..CHANNELS {
                            if value & (1 << i) != 0 {
                                self.start_channel(inner, i);
                            }
                        }
                    }
                    0x34 => {
                        for i in 0..CHANNELS {
                            if value & (1 << i) != 0 {
                                self.stop_channel(i);
                            }
                        }
                    }
                    0x36 => self.tps = value,
                    0x38 => self.to = value,
                    0x3A => self.toe = value,
                    0x3C => self.tol = value,
                    0x3E => self.tom = value,
                    _ => {}
                }
            }
        }
    }

    fn write8(&mut self, bank: TauBank, offset: u64, value: u8) {
        match (bank, offset) {
            (TauBank::Tis, 0) => self.tis0 = value,
            (TauBank::Tis, 1) => self.tis1 = value,
            (TauBank::Tdr01 | TauBank::Tdr27, _) => {}
            (TauBank::Ctrl, 0x10..=0x1E) | (TauBank::Ctrl, 0x36) => {}
            _ => {}
        }
    }
}

struct TauExpire {
    inner: Arc<Mutex<TauUnit>>,
    channel: u8,
    generation: u32,
    at: Tick,
}

impl SimEvent for TauExpire {
    fn fire(&mut self, ctx: &mut EventCtx<'_>) {
        let mut g = self.inner.lock().expect("tau");
        let ch = self.channel as usize;
        if g.generation[ch] != self.generation || !g.channel_enabled(ch) {
            g.expire_id[ch] = None;
            return;
        }
        g.tsr[ch] |= 0x0001;
        g.tcr[ch] = g.tdr[ch];
        let Some(period) = g.interval_ns(ch) else {
            g.expire_id[ch] = None;
            return;
        };
        if period == 0 {
            return;
        }
        let mut next = self.at.saturating_add(Tick(period));
        while next <= ctx.now {
            next = next.saturating_add(Tick(period));
        }
        let id = g.ctl.schedule(
            next,
            Box::new(TauExpire {
                inner: Arc::clone(&self.inner),
                channel: self.channel,
                generation: self.generation,
                at: next,
            }),
        );
        g.expire_id[ch] = Some(id);
    }
}

pub struct TauMmio {
    inner: Arc<Mutex<TauUnit>>,
    bank: TauBank,
}

impl TauMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<TauUnit>>, bank: TauBank) -> Self {
        Self { inner, bank }
    }
}

impl MemoryMapped for TauMmio {
    fn len(&self) -> u64 {
        self.bank.size()
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if offset.saturating_add(buf.len() as u64) > self.bank.size() {
            return Err(BusError::OutOfRange {
                addr: offset,
                offset,
                len: buf.len(),
            });
        }
        let g = self.inner.lock().expect("tau");
        match buf.len() {
            1 => buf[0] = g.read16(self.bank, offset & !1).to_le_bytes()[(offset & 1) as usize],
            2 if offset % 2 == 0 => buf.copy_from_slice(&g.read16(self.bank, offset).to_le_bytes()),
            _ => {
                return Err(BusError::OutOfRange {
                    addr: offset,
                    offset,
                    len: buf.len(),
                });
            }
        }
        Ok(())
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        if offset.saturating_add(buf.len() as u64) > self.bank.size() {
            return Err(BusError::OutOfRange {
                addr: offset,
                offset,
                len: buf.len(),
            });
        }
        let mut g = self.inner.lock().expect("tau");
        match buf.len() {
            1 => g.write8(self.bank, offset, buf[0]),
            2 if offset % 2 == 0 => {
                let value = u16::from_le_bytes([buf[0], buf[1]]);
                g.write16(self.bank, offset, value, &self.inner);
            }
            _ => {
                return Err(BusError::OutOfRange {
                    addr: offset,
                    offset,
                    len: buf.len(),
                });
            }
        }
        Ok(())
    }

    fn host_ptr(&mut self) -> Option<*mut u8> {
        None
    }

    fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::NotLoadable { addr: offset })
    }
}
