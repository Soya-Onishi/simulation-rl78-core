//! Timer Array Unit 0 (QEMU `hw/rl78/tau.c`).
//!
//! Absolute SFR bases are applied by the SoC map, not this module.

use std::sync::{Arc, Mutex};

use sim_kernel::{BusError, EventCtl, EventCtx, EventId, MemoryMapped, Resettable, SimEvent, Tick};

use crate::peripherals::clock::ClockOutputs;

pub const CHANNELS: usize = 8;

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
    clock: ClockOutputs,
    expire_id: [Option<EventId>; CHANNELS],
    generation: [u32; CHANNELS],
}

impl TauUnit {
    #[must_use]
    pub fn new(ctl: EventCtl, clock: ClockOutputs) -> Self {
        Self {
            tdr: [0; CHANNELS],
            tcr: [0; CHANNELS],
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

    fn interval(&self, channel: usize) -> Option<Tick> {
        let cks = ((self.tmr[channel] >> 14) & 0x3) as u8;
        let div = u64::from(self.ck_divider(cks));
        let counts = u64::from(self.tdr[channel]) + 1;
        self.clock
            .f_clk()
            .cycles_to_tick(counts.saturating_mul(div))
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
        let Some(period) = self.interval(ch) else {
            return;
        };
        self.te |= 1 << ch;
        let at = self.ctl.now().saturating_add(period);
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

    fn read_tdr(&self, channel: usize) -> u16 {
        self.tdr[channel]
    }

    fn write_tdr(&mut self, channel: usize, value: u16) {
        self.tdr[channel] = value;
    }

    fn read_ctrl(&self, offset: u64) -> u16 {
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

    fn write_ctrl(&mut self, offset: u64, value: u16, inner: &Arc<Mutex<TauUnit>>) {
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

    fn read_tis(&self, offset: u64) -> u8 {
        match offset {
            0 => self.tis0,
            1 => self.tis1,
            _ => 0,
        }
    }

    fn write_tis(&mut self, offset: u64, value: u8) {
        match offset {
            0 => self.tis0 = value,
            1 => self.tis1 = value,
            _ => {}
        }
    }
}

impl Resettable for TauUnit {
    fn reset(&mut self) {
        for ch in 0..CHANNELS {
            self.stop_channel(ch);
        }
        self.tdr = [0; CHANNELS];
        self.tcr = [0xFFFF; CHANNELS];
        self.tmr = [0; CHANNELS];
        self.tsr = [0; CHANNELS];
        self.tps = 0;
        self.te = 0;
        self.to = 0;
        self.toe = 0;
        self.tol = 0;
        self.tom = 0;
        self.tis0 = 0;
        self.tis1 = 0;
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
        let Some(period) = g.interval(ch) else {
            g.expire_id[ch] = None;
            return;
        };
        if period.is_zero() {
            return;
        }
        let mut next = self.at.saturating_add(period);
        while next <= ctx.now {
            next = next.saturating_add(period);
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

fn oob(offset: u64, len: usize) -> BusError {
    BusError::OutOfRange {
        addr: offset,
        offset,
        len,
    }
}

fn read_word_reg(
    offset: u64,
    buf: &mut [u8],
    size: u64,
    read16: impl Fn(u64) -> u16,
) -> Result<(), BusError> {
    match buf.len() {
        2 if offset % 2 == 0 && offset.saturating_add(2) <= size => {
            buf.copy_from_slice(&read16(offset).to_le_bytes());
            Ok(())
        }
        len => Err(oob(offset, len)),
    }
}

fn write_word_reg(
    offset: u64,
    buf: &[u8],
    size: u64,
    mut write16: impl FnMut(u64, u16),
) -> Result<(), BusError> {
    match buf.len() {
        2 if offset % 2 == 0 && offset.saturating_add(2) <= size => {
            write16(offset, u16::from_le_bytes([buf[0], buf[1]]));
            Ok(())
        }
        len => Err(oob(offset, len)),
    }
}

pub struct TauTdrMmio {
    inner: Arc<Mutex<TauUnit>>,
    channel_base: usize,
    size: u64,
}

impl TauTdrMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<TauUnit>>, channel_base: usize, size: u64) -> Self {
        Self {
            inner,
            channel_base,
            size,
        }
    }
}

impl MemoryMapped for TauTdrMmio {
    fn len(&self) -> u64 {
        self.size
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        let g = self.inner.lock().expect("tau");
        let base = self.channel_base;
        read_word_reg(offset, buf, self.size, |o| {
            g.read_tdr(base + (o / 2) as usize)
        })
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        let mut g = self.inner.lock().expect("tau");
        let base = self.channel_base;
        write_word_reg(offset, buf, self.size, |o, v| {
            g.write_tdr(base + (o / 2) as usize, v)
        })
    }

    fn host_ptr(&mut self) -> Option<*mut u8> {
        None
    }

    fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::NotLoadable { addr: offset })
    }
}

pub struct TauCtrlMmio {
    inner: Arc<Mutex<TauUnit>>,
}

impl TauCtrlMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<TauUnit>>) -> Self {
        Self { inner }
    }
}

impl MemoryMapped for TauCtrlMmio {
    fn len(&self) -> u64 {
        0x40
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        let g = self.inner.lock().expect("tau");
        read_word_reg(offset, buf, 0x40, |o| g.read_ctrl(o))
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        let mut g = self.inner.lock().expect("tau");
        let inner = Arc::clone(&self.inner);
        write_word_reg(offset, buf, 0x40, |o, v| g.write_ctrl(o, v, &inner))
    }

    fn host_ptr(&mut self) -> Option<*mut u8> {
        None
    }

    fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::NotLoadable { addr: offset })
    }
}

pub struct TauTisMmio {
    inner: Arc<Mutex<TauUnit>>,
}

impl TauTisMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<TauUnit>>) -> Self {
        Self { inner }
    }
}

impl MemoryMapped for TauTisMmio {
    fn len(&self) -> u64 {
        2
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if buf.len() != 1 || offset >= 2 {
            return Err(oob(offset, buf.len()));
        }
        let g = self.inner.lock().expect("tau");
        buf[0] = g.read_tis(offset);
        Ok(())
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        if buf.len() != 1 || offset >= 2 {
            return Err(oob(offset, buf.len()));
        }
        let mut g = self.inner.lock().expect("tau");
        g.write_tis(offset, buf[0]);
        Ok(())
    }

    fn host_ptr(&mut self) -> Option<*mut u8> {
        None
    }

    fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::NotLoadable { addr: offset })
    }
}
