//! Serial Array Unit 0 (QEMU `hw/rl78/sau.c`).
//!
//! Absolute SFR bases are applied by the SoC map, not this module.

use std::sync::{Arc, Mutex};

use sim_kernel::{
    BusError, EventCtl, EventCtx, EventId, MemoryMapped, Resettable, SimEvent, SourcePort, Tick,
};

use crate::peripherals::clock::{ClockOutputs, Cycles};

pub const CHANNELS: usize = 4;

const SCR_TXE: u16 = 1 << 15;
const SSR_TSF: u16 = 1 << 6;
const SMR_CKS: u16 = 1 << 15;

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
    ctl: EventCtl,
    clock: ClockOutputs,
    irq_out: [SourcePort<()>; CHANNELS],
    tx_out: SourcePort<u8>,
    tx_id: [Option<EventId>; CHANNELS],
}

impl SauUnit {
    #[must_use]
    pub fn new(ctl: EventCtl, clock: ClockOutputs) -> Self {
        Self {
            sdr: [0; CHANNELS],
            smr: [0; CHANNELS],
            scr: [0; CHANNELS],
            baud_div: [0; CHANNELS],
            ck_divisor: [0, 0],
            se: 0,
            so: 0,
            soe: 0,
            sol: 0,
            busy: [false; CHANNELS],
            ctl,
            clock,
            irq_out: std::array::from_fn(|_| SourcePort::new()),
            tx_out: SourcePort::new(),
            tx_id: [None; CHANNELS],
        }
    }

    #[must_use]
    pub fn irq_source(&mut self, channel: usize) -> &mut SourcePort<()> {
        &mut self.irq_out[channel]
    }

    #[must_use]
    pub fn tx_source(&mut self) -> &mut SourcePort<u8> {
        &mut self.tx_out
    }

    fn cancel_tx(&mut self, channel: usize) {
        if let Some(id) = self.tx_id[channel].take() {
            self.ctl.cancel(id);
        }
        self.busy[channel] = false;
    }

    fn can_start_tx(&self, channel: usize) -> bool {
        self.se & (1 << channel) != 0
            && self.soe & (1 << channel) != 0
            && self.scr[channel] & SCR_TXE != 0
            && !self.busy[channel]
    }

    fn frame_time(&self, channel: usize) -> Option<Tick> {
        let f_clk = self.clock.f_clk();
        if f_clk.is_stopped() {
            return None;
        }
        let prs_sel = if self.smr[channel] & SMR_CKS != 0 {
            1
        } else {
            0
        };
        let prs = u32::from(self.ck_divisor[prs_sel] & 0x0F);
        // Operation clock = f_clk / 2^prs; SDR baud divider; UART 10-bit frame
        // with 2 clocks per bit (QEMU sau.c).
        let cycles = Cycles::from_count(1u64 << prs)
            .saturating_mul(u64::from(self.baud_div[channel]) + 1)
            .saturating_mul(2)
            .saturating_mul(10);
        f_clk.cycles_to_tick(cycles).filter(|t| !t.is_zero())
    }

    fn start_tx(&mut self, inner: &Arc<Mutex<SauUnit>>, channel: usize) {
        if !self.can_start_tx(channel) {
            return;
        }
        let Some(period) = self.frame_time(channel) else {
            return;
        };
        self.cancel_tx(channel);
        self.busy[channel] = true;
        let at = self.ctl.now().saturating_add(period);
        let id = self.ctl.schedule(
            at,
            Box::new(SauTxDone {
                inner: Arc::clone(inner),
                channel: channel as u8,
            }),
        );
        self.tx_id[channel] = Some(id);
    }

    fn read_sdr(&self, channel: usize) -> u16 {
        if self.se & (1 << channel) == 0 {
            self.sdr[channel] | (self.baud_div[channel] << 9)
        } else {
            self.sdr[channel] & 0x1FF
        }
    }

    fn write_sdr(&mut self, channel: usize, value: u16, inner: &Arc<Mutex<SauUnit>>) {
        if self.se & (1 << channel) == 0 {
            self.baud_div[channel] = value >> 9;
        }
        self.sdr[channel] = value & 0x1FF;
        self.start_tx(inner, channel);
    }

    fn read_ctrl(&self, offset: u64) -> u16 {
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

    fn write_ctrl(&mut self, offset: u64, value: u16) {
        match offset {
            0x10 | 0x12 | 0x14 | 0x16 => {
                self.smr[((offset - 0x10) / 2) as usize] = value;
            }
            0x18 | 0x1A | 0x1C | 0x1E => {
                self.scr[((offset - 0x18) / 2) as usize] = value;
            }
            0x22 => self.se |= value & 0x000F,
            0x24 => {
                for ch in 0..CHANNELS {
                    if value & (1 << ch) != 0 {
                        self.se &= !(1 << ch);
                        self.cancel_tx(ch);
                    }
                }
            }
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

impl Resettable for SauUnit {
    fn reset(&mut self) {
        for ch in 0..CHANNELS {
            self.cancel_tx(ch);
        }
        self.sdr = [0; CHANNELS];
        self.smr = [0x0020; CHANNELS];
        self.scr = [0x0004; CHANNELS];
        self.baud_div = [0; CHANNELS];
        self.ck_divisor = [0, 0];
        self.se = 0;
        self.so = 0;
        self.soe = 0;
        self.sol = 0;
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
        g.tx_id[ch] = None;
        let byte = (g.sdr[ch] & 0xFF) as u8;
        g.tx_out.drive(byte);
        g.irq_out[ch].drive(());
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
        1 if offset < size => {
            let w = read16(offset & !1);
            buf[0] = w.to_le_bytes()[(offset & 1) as usize];
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

pub struct SauSdrMmio {
    inner: Arc<Mutex<SauUnit>>,
    channel_base: usize,
}

impl SauSdrMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<SauUnit>>, channel_base: usize) -> Self {
        Self {
            inner,
            channel_base,
        }
    }
}

impl MemoryMapped for SauSdrMmio {
    fn len(&self) -> u64 {
        4
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        let g = self.inner.lock().expect("sau");
        let base = self.channel_base;
        read_word_reg(offset, buf, 4, |o| g.read_sdr(base + (o / 2) as usize))
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        let mut g = self.inner.lock().expect("sau");
        let inner = Arc::clone(&self.inner);
        let base = self.channel_base;
        write_word_reg(offset, buf, 4, |o, v| {
            g.write_sdr(base + (o / 2) as usize, v, &inner)
        })
    }

    fn host_ptr(&mut self) -> Option<*mut u8> {
        None
    }

    fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::NotLoadable { addr: offset })
    }
}

pub struct SauCtrlMmio {
    inner: Arc<Mutex<SauUnit>>,
}

impl SauCtrlMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<SauUnit>>) -> Self {
        Self { inner }
    }
}

impl MemoryMapped for SauCtrlMmio {
    fn len(&self) -> u64 {
        0x40
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        let g = self.inner.lock().expect("sau");
        read_word_reg(offset, buf, 0x40, |o| g.read_ctrl(o))
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        let mut g = self.inner.lock().expect("sau");
        write_word_reg(offset, buf, 0x40, |o, v| g.write_ctrl(o, v))
    }

    fn host_ptr(&mut self) -> Option<*mut u8> {
        None
    }

    fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::NotLoadable { addr: offset })
    }
}
