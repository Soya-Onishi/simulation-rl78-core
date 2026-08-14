//! Timer Array Unit 0 register file (QEMU `hw/rl78/tau.c`).
//!
//! Interval-timer scheduling on the virtual clock is follow-up work.

pub const CHANNELS: usize = 8;

const TDR0: u32 = 0xFFF18;
const TDR2: u32 = 0xFFF64;
const TAU_ESFR: u32 = 0xF0180;
const TIS: u32 = 0xF0074;

#[derive(Clone, Debug)]
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
}

impl Default for TauUnit {
    fn default() -> Self {
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
        }
    }
}

impl TauUnit {
    #[must_use]
    pub fn te(&self) -> u16 {
        self.te
    }

    pub fn read_u8(&self, addr: u32) -> u8 {
        if addr == TIS {
            return self.tis0;
        }
        if addr == TIS + 1 {
            return self.tis1;
        }
        let word = self.read_u16(addr & !1);
        if addr & 1 == 0 {
            word as u8
        } else {
            (word >> 8) as u8
        }
    }

    pub fn write_u8(&mut self, addr: u32, value: u8) {
        if addr == TIS {
            self.tis0 = value;
            return;
        }
        if addr == TIS + 1 {
            self.tis1 = value;
            return;
        }
        let aligned = addr & !1;
        let mut word = self.read_u16(aligned);
        if addr & 1 == 0 {
            word = (word & 0xFF00) | u16::from(value);
        } else {
            word = (word & 0x00FF) | (u16::from(value) << 8);
        }
        self.write_u16(aligned, word);
    }

    fn tdr_channel(addr: u32) -> Option<usize> {
        if (TDR0..TDR0 + 4).contains(&addr) {
            return Some(((addr - TDR0) / 2) as usize);
        }
        if (TDR2..TDR2 + 0x0C).contains(&addr) {
            return Some(2 + ((addr - TDR2) / 2) as usize);
        }
        None
    }

    fn read_u16(&self, addr: u32) -> u16 {
        if let Some(ch) = Self::tdr_channel(addr) {
            return self.tdr[ch];
        }
        let Some(off) = addr.checked_sub(TAU_ESFR) else {
            return 0;
        };
        let ch = ((off / 2) % CHANNELS as u32) as usize;
        match off {
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

    fn write_u16(&mut self, addr: u32, value: u16) {
        if let Some(ch) = Self::tdr_channel(addr) {
            self.tdr[ch] = value;
            return;
        }
        let Some(off) = addr.checked_sub(TAU_ESFR) else {
            return;
        };
        let ch = ((off / 2) % CHANNELS as u32) as usize;
        match off {
            0x00..=0x0E => {}
            0x10..=0x1E => self.tmr[ch] = value,
            0x20..=0x2E => {}
            0x30 => {}
            0x32 => self.te |= value,
            0x34 => self.te &= !value,
            0x36 => self.tps = value,
            0x38 => self.to = value,
            0x3A => self.toe = value,
            0x3C => self.tol = value,
            0x3E => self.tom = value,
            _ => {}
        }
    }

    #[must_use]
    pub fn owns(addr: u32) -> bool {
        Self::tdr_channel(addr).is_some()
            || (TAU_ESFR..TAU_ESFR + 0x40).contains(&addr)
            || addr == TIS
            || addr == TIS + 1
    }
}
