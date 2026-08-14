//! Serial Array Unit 0 (QEMU `hw/rl78/sau.c`).

use std::sync::{Arc, Mutex};

use sim_kernel::{Addr, BusError, EventCtl, EventCtx, MemoryMapped, SimEvent, Tick};

use crate::peripherals::clock::ClockGenerator;

pub const CHANNELS: usize = 4;

const SCR_TXE: u16 = 1 << 15;
const SSR_TSF: u16 = 1 << 6;
const SMR_CKS: u16 = 1 << 15;

/// SAU MMIO windows (QEMU `rl78g23_register_sau`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SauBank {
    /// `0xFFF10`, size 4: SDR0–1.
    Sdr01,
    /// `0xFFF44`, size 4: SDR2–3.
    Sdr23,
    /// `0xF0100`, size 0x40: SSR/SIR/SMR/SCR/SE/SS/ST/SPS/SO/SOE.
    Ctrl,
}

impl SauBank {
    pub const ALL: [Self; 3] = [Self::Sdr01, Self::Sdr23, Self::Ctrl];

    #[must_use]
    pub const fn base(self) -> Addr {
        match self {
            Self::Sdr01 => 0xFFF10,
            Self::Sdr23 => 0xFFF44,
            Self::Ctrl => 0xF0100,
        }
    }

    #[must_use]
    pub const fn size(self) -> u64 {
        match self {
            Self::Sdr01 | Self::Sdr23 => 4,
            Self::Ctrl => 0x40,
        }
    }
}

pub struct SauUnit {
    sdr: [u16; CHANNELS],
    smr: [u16; CHANNELS],
    scr: [u16; CHANNELS],
    baud_div: [u16; CHANNELS],
    ck_divisor: [u8; 2],
    se: u16,
    so: u16,
    soe: u16,
    sol: u16,
    busy: [bool; CHANNELS],
    tx_bytes: Vec<u8>,
    ctl: EventCtl,
    clock: Arc<Mutex<ClockGenerator>>,
}

impl SauUnit {
    #[must_use]
    pub fn new(ctl: EventCtl, clock: Arc<Mutex<ClockGenerator>>) -> Self {
        Self {
            sdr: [0; CHANNELS],
            smr: [0x0020; CHANNELS],
            scr: [0x0004; CHANNELS],
            baud_div: [0; CHANNELS],
            ck_divisor: [0, 0],
            se: 0,
            so: 0,
            soe: 0,
            sol: 0,
            busy: [false; CHANNELS],
            tx_bytes: Vec::new(),
            ctl,
            clock,
        }
    }

    #[must_use]
    pub fn tx_bytes(&self) -> &[u8] {
        &self.tx_bytes
    }

    fn can_start_tx(&self, channel: usize) -> bool {
        self.se & (1 << channel) != 0
            && self.soe & (1 << channel) != 0
            && self.scr[channel] & SCR_TXE != 0
            && !self.busy[channel]
    }

    fn frame_ns(&self, channel: usize) -> Option<u64> {
        let f_clk_hz = self.clock.lock().expect("clock").f_clk_hz();
        if f_clk_hz == 0 {
            return None;
        }
        let prs_sel = if self.smr[channel] & SMR_CKS != 0 {
            1
        } else {
            0
        };
        let prs = u32::from(self.ck_divisor[prs_sel] & 0x0F);
        let fmck = u64::from(f_clk_hz) / (1u64 << prs);
        let div = u64::from(self.baud_div[channel]) + 1;
        let ftclk = fmck / div / 2;
        if ftclk == 0 {
            return None;
        }
        Some(10u64.saturating_mul(1_000_000_000) / ftclk)
    }

    fn start_tx(&mut self, inner: &Arc<Mutex<SauUnit>>, channel: usize) {
        if !self.can_start_tx(channel) {
            return;
        }
        let Some(ns) = self.frame_ns(channel) else {
            return;
        };
        self.busy[channel] = true;
        let at = self.ctl.now().saturating_add(Tick(ns));
        self.ctl.schedule(
            at,
            Box::new(SauTxDone {
                inner: Arc::clone(inner),
                channel: channel as u8,
            }),
        );
    }

    fn sdr_channel(bank: SauBank, offset: u64) -> Option<usize> {
        match bank {
            SauBank::Sdr01 if offset < 4 => Some((offset / 2) as usize),
            SauBank::Sdr23 if offset < 4 => Some(2 + (offset / 2) as usize),
            _ => None,
        }
    }

    fn read16(&self, bank: SauBank, offset: u64) -> u16 {
        if let Some(ch) = Self::sdr_channel(bank, offset) {
            if self.se & (1 << ch) == 0 {
                return self.sdr[ch] | (self.baud_div[ch] << 9);
            }
            return self.sdr[ch] & 0x1FF;
        }
        match offset {
            0x00 | 0x02 | 0x04 | 0x06 => {
                let ch = (offset / 2) as usize;
                if self.busy[ch] { SSR_TSF } else { 0 }
            }
            0x08 | 0x0A | 0x0C | 0x0E => 0,
            0x10 | 0x12 | 0x14 | 0x16 => self.smr[((offset - 0x10) / 2) as usize],
            0x18 | 0x1A | 0x1C | 0x1E => self.scr[((offset - 0x18) / 2) as usize],
            0x20 => self.se,
            0x22 | 0x24 => 0,
            0x26 => {
                u16::from(self.ck_divisor[0] & 0x0F) | (u16::from(self.ck_divisor[1] & 0x0F) << 4)
            }
            0x28 => self.so,
            0x2A => self.soe,
            0x34 => self.sol,
            _ => 0,
        }
    }

    fn write16(&mut self, bank: SauBank, offset: u64, value: u16, inner: &Arc<Mutex<SauUnit>>) {
        if let Some(ch) = Self::sdr_channel(bank, offset) {
            if self.se & (1 << ch) == 0 {
                self.baud_div[ch] = value >> 9;
            }
            self.sdr[ch] = value & 0x1FF;
            self.start_tx(inner, ch);
            return;
        }
        match offset {
            0x10 | 0x12 | 0x14 | 0x16 => {
                self.smr[((offset - 0x10) / 2) as usize] = value;
            }
            0x18 | 0x1A | 0x1C | 0x1E => {
                self.scr[((offset - 0x18) / 2) as usize] = value;
            }
            0x22 => self.se |= value & 0x000F,
            0x24 => self.se &= !(value & 0x000F),
            0x26 => {
                self.ck_divisor[0] = (value & 0x0F) as u8;
                self.ck_divisor[1] = ((value >> 4) & 0x0F) as u8;
            }
            0x28 => self.so = value,
            0x2A => self.soe = value & 0x000F,
            0x34 => self.sol = value & 0x0005,
            _ => {}
        }
    }
}

struct SauTxDone {
    inner: Arc<Mutex<SauUnit>>,
    channel: u8,
}

impl SimEvent for SauTxDone {
    fn fire(&mut self, _ctx: &mut EventCtx<'_>) {
        let mut g = self.inner.lock().expect("sau");
        let ch = self.channel as usize;
        g.busy[ch] = false;
        let byte = (g.sdr[ch] & 0xFF) as u8;
        g.tx_bytes.push(byte);
    }
}

pub struct SauMmio {
    inner: Arc<Mutex<SauUnit>>,
    bank: SauBank,
}

impl SauMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<SauUnit>>, bank: SauBank) -> Self {
        Self { inner, bank }
    }
}

impl MemoryMapped for SauMmio {
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
        let g = self.inner.lock().expect("sau");
        match buf.len() {
            1 => {
                let w = g.read16(self.bank, offset & !1);
                buf[0] = w.to_le_bytes()[(offset & 1) as usize];
            }
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
        let mut g = self.inner.lock().expect("sau");
        match buf.len() {
            2 if offset % 2 == 0 => {
                let value = u16::from_le_bytes([buf[0], buf[1]]);
                g.write16(self.bank, offset, value, &self.inner);
            }
            1 => {
                return Err(BusError::OutOfRange {
                    addr: offset,
                    offset,
                    len: 1,
                });
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
