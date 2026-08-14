//! RL78 clock generator (QEMU `hw/rl78/clock.c`).
//!
//! Registers are offsets within [`ClockBank`] windows mapped on the bus.

use std::sync::{Arc, Mutex};

use sim_kernel::{Addr, BusError, MemoryMapped};

/// Derived oscillator outputs in Hz (0 means stopped).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockTree {
    pub f_ihp: u32,
    pub f_imp: u32,
    pub f_mxp: u32,
    pub f_clk: u32,
    pub f_main: u32,
    pub f_il: u32,
    pub f_sxp: u32,
    pub f_rtcck: u32,
}

/// MMIO windows for the clock generator (QEMU `rl78g23_register_clock`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockBank {
    /// `0xFFFA0`, size 8: CMC..CKSEL.
    Sfr,
    /// `0xF00F2`, size 2: MOCODIV, OSMC.
    OscDiv,
    /// `0xF00A0`, size 9: HIOTRM, HOCODIV.
    Hoco,
    /// `0xF0212`, size 4: MIOTRM..WKUPMD.
    Trim,
}

impl ClockBank {
    pub const ALL: [Self; 4] = [Self::Sfr, Self::OscDiv, Self::Hoco, Self::Trim];

    #[must_use]
    pub const fn base(self) -> Addr {
        match self {
            Self::Sfr => 0xFFFA0,
            Self::OscDiv => 0xF00F2,
            Self::Hoco => 0xF00A0,
            Self::Trim => 0xF0212,
        }
    }

    #[must_use]
    pub const fn size(self) -> u64 {
        match self {
            Self::Sfr => 8,
            Self::OscDiv => 2,
            Self::Hoco => 9,
            Self::Trim => 4,
        }
    }
}

const CSC_HIOSTOP: u8 = 1 << 0;
const CSC_XTSTOP: u8 = 1 << 6;
const CSC_MSTOP: u8 = 1 << 7;
const CSC_WRITABLE: u8 = CSC_HIOSTOP | (1 << 1) | CSC_XTSTOP | CSC_MSTOP;

const CKC_MCM1: u8 = 1 << 0;
const CKC_MCS1: u8 = 1 << 1;
const CKC_MCM0: u8 = 1 << 4;
const CKC_MCS: u8 = 1 << 5;
const CKC_CSS: u8 = 1 << 6;
const CKC_CLS: u8 = 1 << 7;
const CKC_WRITABLE: u8 = CKC_MCM1 | CKC_MCM0 | CKC_CSS;

const CKSEL_SELLOSC: u8 = 1 << 0;
const OSMC_HIPREC: u8 = 1 << 0;
const OSMC_WUTMMCK: u8 = 1 << 4;
const OSMC_WRITABLE: u8 = OSMC_WUTMMCK | (1 << 7);
const WKUPMD_BIT: u8 = 1 << 0;
const OSTS_MASK: u8 = 0x07;
const HOCODIV_MASK: u8 = 0x07;
const MOCODIV_MASK: u8 = 0x03;
const MOSCDIV_MASK: u8 = 0x07;
const HIOTRM_MASK: u8 = 0x3F;

const HIGH_OSC_MHZ: [[u32; 8]; 2] = [[24, 12, 6, 3, 24, 24, 24, 24], [32, 16, 8, 4, 2, 1, 32, 32]];
const MID_OSC_MHZ: [u32; 4] = [4, 2, 1, 4];
const MHZ: u32 = 1_000_000;
const LOW_OSC: u32 = 32_768;

/// Clock generator register file and derived `fCLK` tree.
#[derive(Clone, Debug)]
pub struct ClockGenerator {
    cmc: u8,
    csc: u8,
    osts: u8,
    ckc: u8,
    cks0: u8,
    cks1: u8,
    osmc: u8,
    cksel: u8,
    hocodiv: u8,
    mocodiv: u8,
    moscdiv: u8,
    hiotrm: u8,
    miotrm: u8,
    liotrm: u8,
    wkupmd: u8,
    frqsel3: bool,
    cmc_dirty: bool,
    tree: ClockTree,
}

impl Default for ClockGenerator {
    fn default() -> Self {
        let mut s = Self::new();
        s.reset(None);
        s
    }
}

impl ClockGenerator {
    /// Uninitialized (pre-reset) instance. Apply [`Self::reset`] at MCU reset.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cmc: 0,
            csc: 0,
            osts: 0,
            ckc: 0,
            cks0: 0,
            cks1: 0,
            osmc: 0,
            cksel: 0,
            hocodiv: 0,
            mocodiv: 0,
            moscdiv: 0,
            hiotrm: 0,
            miotrm: 0,
            liotrm: 0,
            wkupmd: 0,
            frqsel3: false,
            cmc_dirty: false,
            tree: ClockTree::default(),
        }
    }

    /// Apply option byte `0x000C2` (`FRQSEL`). `None` uses FRQSEL3=1, HOCODIV=0.
    pub fn reset(&mut self, option_byte: Option<u8>) {
        let (hocodiv, frqsel3) = match option_byte {
            Some(b) => (b & 0x07, (b & 0x08) != 0),
            None => (0, true),
        };
        self.cmc = 0x00;
        self.ckc = 0x00;
        self.csc = 0xC0;
        self.osts = 0x07;
        self.cks0 = 0x00;
        self.cks1 = 0x00;
        self.osmc = 0x01;
        self.cksel = 0x00;
        self.hocodiv = hocodiv;
        self.mocodiv = 0x00;
        self.moscdiv = 0x00;
        self.hiotrm = 0x20;
        self.miotrm = 0x90;
        self.liotrm = 0x80;
        self.wkupmd = 0x00;
        self.frqsel3 = frqsel3;
        self.cmc_dirty = false;
        self.commit();
    }

    #[must_use]
    pub fn tree(&self) -> ClockTree {
        self.tree
    }

    #[must_use]
    pub fn f_clk_hz(&self) -> u32 {
        self.tree.f_clk
    }

    pub fn read_offset(&self, bank: ClockBank, offset: u64) -> u8 {
        match (bank, offset) {
            (ClockBank::Sfr, 0) => self.cmc,
            (ClockBank::Sfr, 1) => self.csc,
            (ClockBank::Sfr, 2) => 0,
            (ClockBank::Sfr, 3) => self.osts,
            (ClockBank::Sfr, 4) => self.read_ckc(),
            (ClockBank::Sfr, 5) => self.cks0,
            (ClockBank::Sfr, 6) => self.cks1,
            (ClockBank::Sfr, 7) => self.cksel,
            (ClockBank::OscDiv, 0) => self.mocodiv,
            (ClockBank::OscDiv, 1) => self.osmc | OSMC_HIPREC,
            (ClockBank::Hoco, 0) => self.hiotrm,
            (ClockBank::Hoco, 8) => self.hocodiv,
            (ClockBank::Trim, 0) => self.miotrm,
            (ClockBank::Trim, 1) => self.liotrm,
            (ClockBank::Trim, 2) => self.moscdiv,
            (ClockBank::Trim, 3) => self.wkupmd,
            _ => 0,
        }
    }

    pub fn write_offset(&mut self, bank: ClockBank, offset: u64, value: u8) {
        match (bank, offset) {
            (ClockBank::Sfr, 0) => {
                if !self.cmc_dirty && self.cmc != value {
                    self.cmc = value;
                    self.cmc_dirty = true;
                }
            }
            (ClockBank::Sfr, 1) => self.csc = value & CSC_WRITABLE,
            (ClockBank::Sfr, 2) => {}
            (ClockBank::Sfr, 3) => self.osts = value & OSTS_MASK,
            (ClockBank::Sfr, 4) => self.write_ckc(value),
            (ClockBank::Sfr, 5) => self.cks0 = value,
            (ClockBank::Sfr, 6) => self.cks1 = value,
            (ClockBank::Sfr, 7) => {
                self.cksel = (value & CKSEL_SELLOSC) | (self.cksel & !CKSEL_SELLOSC);
            }
            (ClockBank::OscDiv, 0) => self.mocodiv = value & MOCODIV_MASK,
            (ClockBank::OscDiv, 1) => {
                self.osmc = (value & OSMC_WRITABLE) | (self.osmc & !OSMC_WRITABLE);
            }
            (ClockBank::Hoco, 0) => self.hiotrm = value & HIOTRM_MASK,
            (ClockBank::Hoco, 8) => self.hocodiv = value & HOCODIV_MASK,
            (ClockBank::Trim, 0) => self.miotrm = value,
            (ClockBank::Trim, 1) => self.liotrm = value,
            (ClockBank::Trim, 2) => self.moscdiv = value & MOSCDIV_MASK,
            (ClockBank::Trim, 3) => {
                self.wkupmd = (value & WKUPMD_BIT) | (self.wkupmd & !WKUPMD_BIT);
            }
            _ => {}
        }
        self.commit();
    }

    fn read_ckc(&self) -> u8 {
        self.with_status_bits(self.ckc)
    }

    fn write_ckc(&mut self, value: u8) {
        self.ckc = (self.ckc & !CKC_WRITABLE) | (value & CKC_WRITABLE);
        self.ckc = self.with_status_bits(self.ckc);
    }

    fn with_status_bits(&self, ckc: u8) -> u8 {
        let mut out = ckc;
        if out & CKC_CSS != 0 {
            out |= CKC_CLS;
        } else {
            out &= !CKC_CLS;
        }
        if out & CKC_MCM0 != 0 {
            out |= CKC_MCS;
        } else {
            out &= !CKC_MCS;
        }
        if out & CKC_MCM1 != 0 {
            out |= CKC_MCS1;
        } else {
            out &= !CKC_MCS1;
        }
        out
    }

    fn commit(&mut self) {
        let high_mhz = HIGH_OSC_MHZ[usize::from(self.frqsel3)][(self.hocodiv & 7) as usize];
        let mid_mhz = MID_OSC_MHZ[(self.mocodiv & 3) as usize];
        let high_osc = if self.csc & CSC_HIOSTOP != 0 {
            0
        } else {
            high_mhz * MHZ
        };
        let mid_osc = if self.csc & (1 << 1) != 0 {
            mid_mhz * MHZ
        } else {
            0
        };
        let f_sub = if self.cksel & CKSEL_SELLOSC != 0 {
            LOW_OSC
        } else {
            0
        };
        let f_oco = if self.ckc & CKC_MCM1 != 0 {
            mid_osc
        } else {
            high_osc
        };
        let f_main = if self.ckc & CKC_MCM0 != 0 { 0 } else { f_oco };
        let f_clk = if self.ckc & CKC_CSS != 0 {
            f_sub
        } else {
            f_main
        };
        let f_sxp = if self.osmc & OSMC_WUTMMCK != 0 {
            LOW_OSC
        } else {
            0
        };
        self.tree = ClockTree {
            f_ihp: high_osc,
            f_imp: mid_osc,
            f_mxp: 0,
            f_clk,
            f_main,
            f_il: LOW_OSC,
            f_sxp,
            f_rtcck: f_sxp,
        };
    }
}

/// One mapped clock window. 8-bit SFRs: a 16-bit access updates two adjacent
/// registers once each (not a 16-bit timer-style RMW).
pub struct ClockMmio {
    inner: Arc<Mutex<ClockGenerator>>,
    bank: ClockBank,
}

impl ClockMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<ClockGenerator>>, bank: ClockBank) -> Self {
        Self { inner, bank }
    }
}

impl MemoryMapped for ClockMmio {
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
        let g = self.inner.lock().expect("clock");
        for (i, slot) in buf.iter_mut().enumerate() {
            *slot = g.read_offset(self.bank, offset + i as u64);
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
        let mut g = self.inner.lock().expect("clock");
        for (i, value) in buf.iter().enumerate() {
            g.write_offset(self.bank, offset + i as u64, *value);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_fclk_is_hoco_32mhz() {
        let c = ClockGenerator::default();
        assert_eq!(c.f_clk_hz(), 32 * MHZ);
        assert_eq!(c.read_offset(ClockBank::Sfr, 1), 0xC0);
        assert_eq!(c.read_offset(ClockBank::Sfr, 4), 0x00);
    }

    #[test]
    fn hocodiv_halves_hoco() {
        let mut c = ClockGenerator::default();
        c.write_offset(ClockBank::Hoco, 8, 1);
        assert_eq!(c.f_clk_hz(), 16 * MHZ);
    }

    #[test]
    fn hiostop_kills_fclk() {
        let mut c = ClockGenerator::default();
        c.write_offset(ClockBank::Sfr, 1, CSC_HIOSTOP | CSC_XTSTOP | CSC_MSTOP);
        assert_eq!(c.f_clk_hz(), 0);
    }

    #[test]
    fn ckc_css_mirrors_to_cls() {
        let mut c = ClockGenerator::default();
        c.write_offset(ClockBank::Sfr, 7, CKSEL_SELLOSC);
        c.write_offset(ClockBank::Sfr, 4, CKC_CSS);
        let v = c.read_offset(ClockBank::Sfr, 4);
        assert_ne!(v & CKC_CLS, 0);
        assert_ne!(v & CKC_CSS, 0);
        assert_eq!(c.f_clk_hz(), LOW_OSC);
    }

    #[test]
    fn css_without_xt1_or_sellosc_stops_fclk() {
        let mut c = ClockGenerator::default();
        c.write_offset(ClockBank::Sfr, 4, CKC_CSS);
        assert_eq!(c.f_clk_hz(), 0);
    }

    #[test]
    fn new_does_not_apply_option_byte() {
        let c = ClockGenerator::new();
        assert_eq!(c.f_clk_hz(), 0);
        let mut c = ClockGenerator::new();
        c.reset(None);
        assert_eq!(c.f_clk_hz(), 32 * MHZ);
    }
}
