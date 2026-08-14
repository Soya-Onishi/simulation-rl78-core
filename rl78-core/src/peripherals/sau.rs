//! Serial Array Unit 0 register file (QEMU `hw/rl78/sau.c`).
//!
//! UART bit timing and IRQ delivery are follow-up work; this stores the
//! programmer-visible registers so guest init sequences can run.

pub const CHANNELS: usize = 4;

const SDR0: u32 = 0xFFF10;
const SDR2: u32 = 0xFFF44;
const SAU_ESFR: u32 = 0xF0100;

#[derive(Clone, Debug)]
pub struct SauUnit {
    sdr: [u16; CHANNELS],
    smr: [u16; CHANNELS],
    scr: [u16; CHANNELS],
    ssr: [u16; CHANNELS],
    ck_divisor: [u8; 2],
    se: u16,
    so: u16,
    soe: u16,
    sol: u16,
}

impl Default for SauUnit {
    fn default() -> Self {
        Self {
            sdr: [0; CHANNELS],
            smr: [0x0020; CHANNELS],
            scr: [0x0004; CHANNELS],
            ssr: [0; CHANNELS],
            ck_divisor: [0, 0],
            se: 0,
            so: 0,
            soe: 0,
            sol: 0,
        }
    }
}

impl SauUnit {
    #[must_use]
    pub fn se(&self) -> u16 {
        self.se
    }

    pub fn read_u8(&self, addr: u32) -> u8 {
        let word = self.read_u16(addr & !1);
        if addr & 1 == 0 {
            word as u8
        } else {
            (word >> 8) as u8
        }
    }

    pub fn write_u8(&mut self, addr: u32, value: u8) {
        let aligned = addr & !1;
        let mut word = self.read_u16(aligned);
        if addr & 1 == 0 {
            word = (word & 0xFF00) | u16::from(value);
        } else {
            word = (word & 0x00FF) | (u16::from(value) << 8);
        }
        self.write_u16(aligned, word);
    }

    fn read_u16(&self, addr: u32) -> u16 {
        if let Some(ch) = sdr_channel(addr) {
            return self.sdr[ch] & 0x1FF;
        }
        let Some(off) = addr.checked_sub(SAU_ESFR) else {
            return 0;
        };
        match off {
            0x00 | 0x02 | 0x04 | 0x06 => self.ssr[(off / 2) as usize],
            0x08 | 0x0A | 0x0C | 0x0E => 0,
            0x10 | 0x12 | 0x14 | 0x16 => self.smr[((off - 0x10) / 2) as usize],
            0x18 | 0x1A | 0x1C | 0x1E => self.scr[((off - 0x18) / 2) as usize],
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

    fn write_u16(&mut self, addr: u32, value: u16) {
        if let Some(ch) = sdr_channel(addr) {
            self.sdr[ch] = value;
            return;
        }
        let Some(off) = addr.checked_sub(SAU_ESFR) else {
            return;
        };
        match off {
            0x00 | 0x02 | 0x04 | 0x06 => {
                self.ssr[(off / 2) as usize] = value;
            }
            0x08 | 0x0A | 0x0C | 0x0E => {
                let ch = ((off - 0x08) / 2) as usize;
                if value & 1 != 0 {
                    self.ssr[ch] &= !0x0001;
                }
                if value & 2 != 0 {
                    self.ssr[ch] &= !0x0002;
                }
                if value & 4 != 0 {
                    self.ssr[ch] &= !0x0004;
                }
            }
            0x10 | 0x12 | 0x14 | 0x16 => {
                self.smr[((off - 0x10) / 2) as usize] = value;
            }
            0x18 | 0x1A | 0x1C | 0x1E => {
                self.scr[((off - 0x18) / 2) as usize] = value;
            }
            0x20 => {}
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

    #[must_use]
    pub fn owns(addr: u32) -> bool {
        sdr_channel(addr).is_some() || (SAU_ESFR..SAU_ESFR + 0x40).contains(&addr)
    }
}

fn sdr_channel(addr: u32) -> Option<usize> {
    if (SDR0..SDR0 + 2).contains(&addr) {
        Some(0)
    } else if (SDR0 + 2..SDR0 + 4).contains(&addr) {
        Some(1)
    } else if (SDR2..SDR2 + 2).contains(&addr) {
        Some(2)
    } else if (SDR2 + 2..SDR2 + 4).contains(&addr) {
        Some(3)
    } else {
        None
    }
}
