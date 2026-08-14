//! RL78 clock generator (QEMU `hw/rl78/clock.c`).

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

const CMC: u32 = 0xFFFA0;
const CSC: u32 = 0xFFFA1;
const OSTC: u32 = 0xFFFA2;
const OSTS: u32 = 0xFFFA3;
const CKC: u32 = 0xFFFA4;
const CKS0: u32 = 0xFFFA5;
const CKS1: u32 = 0xFFFA6;
const CKSEL: u32 = 0xFFFA7;
const MOCODIV: u32 = 0xF00F2;
const OSMC: u32 = 0xF00F3;
const HIOTRM: u32 = 0xF00A0;
const HOCODIV: u32 = 0xF00A8;
const MIOTRM: u32 = 0xF0212;
const LIOTRM: u32 = 0xF0213;
const MOSCDIV: u32 = 0xF0214;
const WKUPMD: u32 = 0xF0215;

const CSC_HIOSTOP: u8 = 1 << 0;
const CSC_MIOEN: u8 = 1 << 1;
const CSC_XTSTOP: u8 = 1 << 6;
const CSC_MSTOP: u8 = 1 << 7;
const CSC_WRITABLE: u8 = CSC_HIOSTOP | CSC_MIOEN | CSC_XTSTOP | CSC_MSTOP;

const CKC_MCM1: u8 = 1 << 0;
const CKC_MCS1: u8 = 1 << 1;
const CKC_MCM0: u8 = 1 << 4;
const CKC_MCS: u8 = 1 << 5;
const CKC_CSS: u8 = 1 << 6;
const CKC_CLS: u8 = 1 << 7;
/// Hardware-writable bits (MCM1 / MCM0 / CSS). Status bits mirror these.
const CKC_WRITABLE: u8 = CKC_MCM1 | CKC_MCM0 | CKC_CSS;

const CKSEL_SELLOSC: u8 = 1 << 0;
const OSMC_HIPREC: u8 = 1 << 0;
const OSMC_WUTMMCK: u8 = 1 << 4;
const OSMC_RTCLPC: u8 = 1 << 7;
const OSMC_WRITABLE: u8 = OSMC_WUTMMCK | OSMC_RTCLPC;
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
        Self::from_option_byte(None)
    }
}

impl ClockGenerator {
    /// `option_byte` is flash `0x000C2` (QEMU reset). `None` uses FRQSEL3=1, HOCODIV=0.
    #[must_use]
    pub fn from_option_byte(option_byte: Option<u8>) -> Self {
        let (hocodiv, frqsel3) = match option_byte {
            Some(b) => (b & 0x07, (b & 0x08) != 0),
            None => (0, true),
        };
        let mut s = Self {
            cmc: 0x00,
            ckc: 0x00,
            csc: 0xC0,
            osts: 0x07,
            cks0: 0x00,
            cks1: 0x00,
            osmc: 0x01,
            cksel: 0x00,
            hocodiv,
            mocodiv: 0x00,
            moscdiv: 0x00,
            hiotrm: 0x20,
            miotrm: 0x90,
            liotrm: 0x80,
            wkupmd: 0x00,
            frqsel3,
            cmc_dirty: false,
            tree: ClockTree::default(),
        };
        s.commit();
        s
    }

    #[must_use]
    pub fn tree(&self) -> ClockTree {
        self.tree
    }

    #[must_use]
    pub fn f_clk_hz(&self) -> u32 {
        self.tree.f_clk
    }

    pub fn read_u8(&self, addr: u32) -> u8 {
        match addr {
            CMC => self.cmc,
            CSC => self.csc,
            OSTC => 0,
            OSTS => self.osts,
            CKC => self.read_ckc(),
            CKS0 => self.cks0,
            CKS1 => self.cks1,
            CKSEL => self.cksel,
            MOCODIV => self.mocodiv,
            OSMC => self.osmc | OSMC_HIPREC,
            HIOTRM => self.hiotrm,
            HOCODIV => self.hocodiv,
            MIOTRM => self.miotrm,
            LIOTRM => self.liotrm,
            MOSCDIV => self.moscdiv,
            WKUPMD => self.wkupmd,
            _ => 0,
        }
    }

    pub fn write_u8(&mut self, addr: u32, value: u8) {
        match addr {
            CMC => {
                if !self.cmc_dirty && self.cmc != value {
                    self.cmc = value;
                    self.cmc_dirty = true;
                }
            }
            CSC => self.csc = value & CSC_WRITABLE,
            OSTC => {}
            OSTS => self.osts = value & OSTS_MASK,
            CKC => self.write_ckc(value),
            CKS0 => self.cks0 = value,
            CKS1 => self.cks1 = value,
            CKSEL => {
                self.cksel = (value & CKSEL_SELLOSC) | (self.cksel & !CKSEL_SELLOSC);
            }
            MOCODIV => self.mocodiv = value & MOCODIV_MASK,
            OSMC => {
                self.osmc = (value & OSMC_WRITABLE) | (self.osmc & !OSMC_WRITABLE);
            }
            HIOTRM => self.hiotrm = value & HIOTRM_MASK,
            HOCODIV => self.hocodiv = value & HOCODIV_MASK,
            MIOTRM => self.miotrm = value,
            LIOTRM => self.liotrm = value,
            MOSCDIV => self.moscdiv = value & MOSCDIV_MASK,
            WKUPMD => {
                self.wkupmd = (value & WKUPMD_BIT) | (self.wkupmd & !WKUPMD_BIT);
            }
            _ => {}
        }
        self.commit();
    }

    #[must_use]
    pub fn owns(addr: u32) -> bool {
        matches!(
            addr,
            CMC | CSC
                | OSTC
                | OSTS
                | CKC
                | CKS0
                | CKS1
                | CKSEL
                | MOCODIV
                | OSMC
                | HIOTRM
                | HOCODIV
                | MIOTRM
                | LIOTRM
                | MOSCDIV
                | WKUPMD
        )
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
        let mid_osc = if self.csc & CSC_MIOEN != 0 {
            mid_mhz * MHZ
        } else {
            0
        };
        // SELLOSC=1 selects LOCO as fSUB (G23). SELLOSC=0 is XT1, unimplemented → 0.
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
        // Datasheet: CSS selects fCLK (fMAIN vs fSUB). QEMU `clock.c` uses MCS here.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_fclk_is_hoco_32mhz() {
        let c = ClockGenerator::default();
        assert_eq!(c.f_clk_hz(), 32 * MHZ);
        assert_eq!(c.read_u8(CSC), 0xC0);
        assert_eq!(c.read_u8(CKC), 0x00);
    }

    #[test]
    fn hocodiv_halves_hoco() {
        let mut c = ClockGenerator::default();
        c.write_u8(HOCODIV, 1);
        assert_eq!(c.f_clk_hz(), 16 * MHZ);
    }

    #[test]
    fn hiostop_kills_fclk() {
        let mut c = ClockGenerator::default();
        c.write_u8(CSC, CSC_HIOSTOP | CSC_XTSTOP | CSC_MSTOP);
        assert_eq!(c.f_clk_hz(), 0);
    }

    #[test]
    fn ckc_css_mirrors_to_cls() {
        let mut c = ClockGenerator::default();
        c.write_u8(CKSEL, CKSEL_SELLOSC);
        c.write_u8(CKC, CKC_CSS);
        let v = c.read_u8(CKC);
        assert_ne!(v & CKC_CLS, 0);
        assert_ne!(v & CKC_CSS, 0);
        assert_eq!(c.f_clk_hz(), LOW_OSC);
    }

    #[test]
    fn css_without_xt1_or_sellosc_stops_fclk() {
        let mut c = ClockGenerator::default();
        c.write_u8(CKC, CKC_CSS);
        assert_eq!(c.f_clk_hz(), 0);
    }
}
