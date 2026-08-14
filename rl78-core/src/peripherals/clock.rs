//! RL78 clock generator (QEMU `hw/rl78/clock.c`).
//!
//! Absolute SFR bases are applied by the SoC map, not this module.

use std::sync::{Arc, Mutex};

use sim_kernel::{BusError, MemoryMapped, Resettable, Tick};

/// Frequency in hertz. `ZERO` means the oscillator is stopped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hertz(u32);

impl Hertz {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn from_hz(hz: u32) -> Self {
        Self(hz)
    }

    #[must_use]
    pub const fn from_mhz(mhz: u32) -> Self {
        Self(mhz.saturating_mul(1_000_000))
    }

    #[must_use]
    pub const fn is_stopped(self) -> bool {
        self.0 == 0
    }

    /// `cycles` of this clock as virtual nanoseconds.
    #[must_use]
    pub const fn cycles_to_tick(self, cycles: u64) -> Option<Tick> {
        if self.0 == 0 {
            None
        } else {
            Some(Tick(cycles.saturating_mul(1_000_000_000) / (self.0 as u64)))
        }
    }
}

/// Derived oscillator outputs (0 means stopped).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockTree {
    pub f_ihp: Hertz,
    pub f_imp: Hertz,
    pub f_mxp: Hertz,
    pub f_clk: Hertz,
    pub f_main: Hertz,
    pub f_il: Hertz,
    pub f_sxp: Hertz,
    pub f_rtcck: Hertz,
}

/// Shared clock *outputs* (QEMU `Clock` out ports), not the generator itself.
#[derive(Clone)]
pub struct ClockOutputs {
    tree: Arc<Mutex<ClockTree>>,
}

impl ClockOutputs {
    fn new() -> Self {
        Self {
            tree: Arc::new(Mutex::new(ClockTree::default())),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> ClockTree {
        *self.tree.lock().expect("clock outputs")
    }

    #[must_use]
    pub fn f_clk(&self) -> Hertz {
        self.snapshot().f_clk
    }

    fn publish(&self, tree: ClockTree) {
        *self.tree.lock().expect("clock outputs") = tree;
    }
}

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
const LOW_OSC: Hertz = Hertz::from_hz(32_768);

/// Clock generator register file. Outputs are published on [`ClockOutputs`].
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
    outputs: ClockOutputs,
    option_byte: Option<u8>,
}

impl Default for ClockGenerator {
    fn default() -> Self {
        let mut s = Self::new();
        Resettable::reset(&mut s);
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
            outputs: ClockOutputs::new(),
            option_byte: None,
        }
    }

    #[must_use]
    pub fn outputs(&self) -> ClockOutputs {
        self.outputs.clone()
    }

    pub fn set_option_byte(&mut self, option_byte: Option<u8>) {
        self.option_byte = option_byte;
    }

    #[must_use]
    pub fn tree(&self) -> ClockTree {
        self.outputs.snapshot()
    }

    #[must_use]
    pub fn f_clk(&self) -> Hertz {
        self.outputs.f_clk()
    }

    pub fn read_sfr(&self, offset: u64) -> u8 {
        match offset {
            0 => self.cmc,
            1 => self.csc,
            2 => 0,
            3 => self.osts,
            4 => self.read_ckc(),
            5 => self.cks0,
            6 => self.cks1,
            7 => self.cksel,
            _ => 0,
        }
    }

    pub fn write_sfr(&mut self, offset: u64, value: u8) {
        match offset {
            0 => {
                if !self.cmc_dirty && self.cmc != value {
                    self.cmc = value;
                    self.cmc_dirty = true;
                }
            }
            1 => self.csc = value & CSC_WRITABLE,
            2 => {}
            3 => self.osts = value & OSTS_MASK,
            4 => self.write_ckc(value),
            5 => self.cks0 = value,
            6 => self.cks1 = value,
            7 => {
                self.cksel = (value & CKSEL_SELLOSC) | (self.cksel & !CKSEL_SELLOSC);
            }
            _ => {}
        }
        self.commit();
    }

    pub fn read_osc_div(&self, offset: u64) -> u8 {
        match offset {
            0 => self.mocodiv,
            1 => self.osmc | OSMC_HIPREC,
            _ => 0,
        }
    }

    pub fn write_osc_div(&mut self, offset: u64, value: u8) {
        match offset {
            0 => self.mocodiv = value & MOCODIV_MASK,
            1 => self.osmc = (value & OSMC_WRITABLE) | (self.osmc & !OSMC_WRITABLE),
            _ => {}
        }
        self.commit();
    }

    pub fn read_hoco(&self, offset: u64) -> u8 {
        match offset {
            0 => self.hiotrm,
            8 => self.hocodiv,
            _ => 0,
        }
    }

    pub fn write_hoco(&mut self, offset: u64, value: u8) {
        match offset {
            0 => self.hiotrm = value & HIOTRM_MASK,
            8 => self.hocodiv = value & HOCODIV_MASK,
            _ => {}
        }
        self.commit();
    }

    pub fn read_trim(&self, offset: u64) -> u8 {
        match offset {
            0 => self.miotrm,
            1 => self.liotrm,
            2 => self.moscdiv,
            3 => self.wkupmd,
            _ => 0,
        }
    }

    pub fn write_trim(&mut self, offset: u64, value: u8) {
        match offset {
            0 => self.miotrm = value,
            1 => self.liotrm = value,
            2 => self.moscdiv = value & MOSCDIV_MASK,
            3 => self.wkupmd = (value & WKUPMD_BIT) | (self.wkupmd & !WKUPMD_BIT),
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
            Hertz::ZERO
        } else {
            Hertz::from_mhz(high_mhz)
        };
        let mid_osc = if self.csc & CSC_MIOEN != 0 {
            Hertz::from_mhz(mid_mhz)
        } else {
            Hertz::ZERO
        };
        let f_sub = if self.cksel & CKSEL_SELLOSC != 0 {
            LOW_OSC
        } else {
            Hertz::ZERO
        };
        let f_oco = if self.ckc & CKC_MCM1 != 0 {
            mid_osc
        } else {
            high_osc
        };
        let f_main = if self.ckc & CKC_MCM0 != 0 {
            Hertz::ZERO
        } else {
            f_oco
        };
        let f_clk = if self.ckc & CKC_CSS != 0 {
            f_sub
        } else {
            f_main
        };
        let f_sxp = if self.osmc & OSMC_WUTMMCK != 0 {
            LOW_OSC
        } else {
            Hertz::ZERO
        };
        self.outputs.publish(ClockTree {
            f_ihp: high_osc,
            f_imp: mid_osc,
            f_mxp: Hertz::ZERO,
            f_clk,
            f_main,
            f_il: LOW_OSC,
            f_sxp,
            f_rtcck: f_sxp,
        });
    }
}

impl Resettable for ClockGenerator {
    fn reset(&mut self) {
        let (hocodiv, frqsel3) = match self.option_byte {
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
}

fn oob(offset: u64, len: usize) -> BusError {
    BusError::OutOfRange {
        addr: offset,
        offset,
        len,
    }
}

/// 8-bit-only SFR window (QEMU `impl.max_access_size = 1`).
/// A 16-bit bus op is two adjacent 8-bit registers, not one 16-bit register.
fn read_u8_window(
    offset: u64,
    buf: &mut [u8],
    size: u64,
    read8: impl Fn(u64) -> u8,
) -> Result<(), BusError> {
    match buf.len() {
        1 if offset < size => {
            buf[0] = read8(offset);
            Ok(())
        }
        2 if offset.saturating_add(2) <= size => {
            buf[0] = read8(offset);
            buf[1] = read8(offset + 1);
            Ok(())
        }
        len => Err(oob(offset, len)),
    }
}

fn write_u8_window(
    offset: u64,
    buf: &[u8],
    size: u64,
    mut write8: impl FnMut(u64, u8),
) -> Result<(), BusError> {
    match buf.len() {
        1 if offset < size => {
            write8(offset, buf[0]);
            Ok(())
        }
        2 if offset.saturating_add(2) <= size => {
            write8(offset, buf[0]);
            write8(offset + 1, buf[1]);
            Ok(())
        }
        len => Err(oob(offset, len)),
    }
}

macro_rules! clock_byte_mmio {
    ($name:ident, $size:expr, $read:ident, $write:ident) => {
        pub struct $name {
            inner: Arc<Mutex<ClockGenerator>>,
        }

        impl $name {
            #[must_use]
            pub fn new(inner: Arc<Mutex<ClockGenerator>>) -> Self {
                Self { inner }
            }
        }

        impl MemoryMapped for $name {
            fn len(&self) -> u64 {
                $size
            }

            fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
                let g = self.inner.lock().expect("clock");
                read_u8_window(offset, buf, $size, |o| g.$read(o))
            }

            fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
                let mut g = self.inner.lock().expect("clock");
                write_u8_window(offset, buf, $size, |o, v| g.$write(o, v))
            }

            fn host_ptr(&mut self) -> Option<*mut u8> {
                None
            }

            fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
                Err(BusError::NotLoadable { addr: offset })
            }
        }
    };
}

clock_byte_mmio!(ClockSfrMmio, 8, read_sfr, write_sfr);
clock_byte_mmio!(ClockOscDivMmio, 2, read_osc_div, write_osc_div);
clock_byte_mmio!(ClockHocoMmio, 9, read_hoco, write_hoco);
clock_byte_mmio!(ClockTrimMmio, 4, read_trim, write_trim);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_fclk_is_hoco_32mhz() {
        let c = ClockGenerator::default();
        assert_eq!(c.f_clk(), Hertz::from_mhz(32));
        assert_eq!(c.read_sfr(1), 0xC0);
        assert_eq!(c.read_sfr(4), 0x00);
    }

    #[test]
    fn hocodiv_halves_hoco() {
        let mut c = ClockGenerator::default();
        c.write_hoco(8, 1);
        assert_eq!(c.f_clk(), Hertz::from_mhz(16));
    }

    #[test]
    fn hiostop_kills_fclk() {
        let mut c = ClockGenerator::default();
        c.write_sfr(1, CSC_HIOSTOP | CSC_XTSTOP | CSC_MSTOP);
        assert_eq!(c.f_clk(), Hertz::ZERO);
    }

    #[test]
    fn ckc_css_mirrors_to_cls() {
        let mut c = ClockGenerator::default();
        c.write_sfr(7, CKSEL_SELLOSC);
        c.write_sfr(4, CKC_CSS);
        let v = c.read_sfr(4);
        assert_ne!(v & CKC_CLS, 0);
        assert_ne!(v & CKC_CSS, 0);
        assert_eq!(c.f_clk(), LOW_OSC);
    }

    #[test]
    fn css_without_xt1_or_sellosc_stops_fclk() {
        let mut c = ClockGenerator::default();
        c.write_sfr(4, CKC_CSS);
        assert_eq!(c.f_clk(), Hertz::ZERO);
    }

    #[test]
    fn new_does_not_apply_option_byte() {
        let c = ClockGenerator::new();
        assert_eq!(c.f_clk(), Hertz::ZERO);
        let mut c = ClockGenerator::new();
        Resettable::reset(&mut c);
        assert_eq!(c.f_clk(), Hertz::from_mhz(32));
    }
}
