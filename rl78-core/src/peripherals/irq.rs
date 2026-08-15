//! Interrupt controller (QEMU `hw/rl78/irq.c`).
//!
//! Absolute SFR bases are applied by the SoC map. When a CPU line is bound
//! ([`bind_cpu_line`]), [`IrqController::pending`] is forwarded to
//! `tlib_set_rl78_irq`. `tlib_on_rl78_irq_ack` clears the accepted IF and
//! re-evaluates the next request.

use std::sync::{Arc, Mutex, OnceLock};

use sim_kernel::{BusError, MemoryMapped, Resettable};

use crate::ffi;

fn cpu_line() -> &'static Mutex<Option<Arc<Mutex<IrqController>>>> {
    static LINE: OnceLock<Mutex<Option<Arc<Mutex<IrqController>>>>> = OnceLock::new();
    LINE.get_or_init(|| Mutex::new(None))
}

/// Attach this INTC to the live tlib CPU (sim thread / one `Rl78Cpu`).
pub(crate) fn bind_cpu_line(irq: Arc<Mutex<IrqController>>) {
    let pending = irq.lock().expect("irq").pending();
    *cpu_line().lock().expect("irq cpu line") = Some(irq);
    apply_tlib_irq(pending);
}

pub(crate) fn unbind_cpu_line() {
    let mut slot = cpu_line().lock().expect("irq cpu line");
    if slot.take().is_some() {
        apply_tlib_irq(None);
    }
}

fn irq_cpu_bound() -> bool {
    cpu_line().lock().expect("irq cpu line").is_some()
}

fn drive_cpu(pending: Option<IrqRequest>) {
    if !irq_cpu_bound() {
        return;
    }
    apply_tlib_irq(pending);
}

fn apply_tlib_irq(pending: Option<IrqRequest>) {
    unsafe {
        match pending {
            Some(req) => {
                ffi::tlib_set_rl78_irq(i32::from(req.index.index()), i32::from(req.priority), 1)
            }
            None => ffi::tlib_set_rl78_irq(0, 0, 0),
        }
    }
}

pub(crate) fn on_tlib_ack(index: u32) {
    let irq = cpu_line().lock().expect("irq cpu line").clone();
    let Some(irq) = irq else {
        return;
    };
    let Some(id) = IrqId::from_index(index as u8) else {
        return;
    };
    irq.lock().expect("irq").ack(id);
}

/// Vector index (QEMU `RL78CPUIRQ`). Aliases that share a vector keep one name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IrqId(u8);

impl IrqId {
    pub const INTWDTI: Self = Self(0);
    pub const INTLVI: Self = Self(1);
    pub const INTP0: Self = Self(2);
    pub const INTP1: Self = Self(3);
    pub const INTP2: Self = Self(4);
    pub const INTP3: Self = Self(5);
    pub const INTP4: Self = Self(6);
    pub const INTP5: Self = Self(7);
    pub const INTST2: Self = Self(8);
    pub const INTSR2: Self = Self(9);
    pub const INTSRE2: Self = Self(10);
    pub const INTELCL: Self = Self(11);
    pub const INTSMSE: Self = Self(12);
    pub const INTST0: Self = Self(13);
    pub const INTTM00: Self = Self(14);
    pub const INTSRE0: Self = Self(15);
    pub const INTST1: Self = Self(16);
    pub const INTSR1: Self = Self(17);
    pub const INTSRE1: Self = Self(18);
    pub const INTIICA0: Self = Self(19);
    pub const INTSR0: Self = Self(20);
    pub const INTTM01: Self = Self(21);
    pub const INTTM02: Self = Self(22);
    pub const INTTM03: Self = Self(23);
    pub const INTAD: Self = Self(24);
    pub const INTRTC: Self = Self(25);
    pub const INTITL: Self = Self(26);
    pub const INTKR: Self = Self(27);
    pub const INTST3: Self = Self(28);
    pub const INTSR3: Self = Self(29);
    pub const INTTM13: Self = Self(30);
    pub const INTTM04: Self = Self(31);
    pub const INTTM05: Self = Self(32);
    pub const INTTM06: Self = Self(33);
    pub const INTTM07: Self = Self(34);
    pub const INTP6: Self = Self(35);
    pub const INTP7: Self = Self(36);
    pub const INTP8: Self = Self(37);
    pub const INTP9: Self = Self(38);
    pub const INTFL: Self = Self(39);
    pub const INTP10: Self = Self(40);
    pub const INTP11: Self = Self(41);
    pub const INTURE0: Self = Self(42);
    pub const INTURE1: Self = Self(43);
    pub const INTTM12: Self = Self(44);
    pub const INTSRE3: Self = Self(45);
    pub const INTCTSUWR: Self = Self(46);
    pub const INTIICA1: Self = Self(47);
    pub const INTCTSURD: Self = Self(48);
    pub const INTCTSUFN: Self = Self(49);
    pub const INTREMC: Self = Self(50);
    pub const INTUT0: Self = Self(51);
    pub const INTUR0: Self = Self(52);
    pub const INTUT1: Self = Self(53);
    pub const INTUR1: Self = Self(54);
    pub const INTTM14: Self = Self(55);
    pub const INTTM15: Self = Self(56);
    pub const INTTM16: Self = Self(57);
    pub const INTTM17: Self = Self(58);

    pub const NUM: u8 = 59;
    pub const PRIORITY_LEVELS: u8 = 4;
    const SLOTS: usize = 64;
    const EXT_PINS: usize = 12;

    #[must_use]
    pub const fn index(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn from_index(index: u8) -> Option<Self> {
        if index < Self::NUM {
            Some(Self(index))
        } else {
            None
        }
    }
}

/// Highest-priority unmasked request (QEMU `rl78_irq_set_irq` packing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqRequest {
    pub index: IrqId,
    pub priority: u8,
}

/// Shared raise port for TAU/SAU (QEMU `irq-in`).
#[derive(Clone)]
pub struct IrqSink {
    inner: Arc<Mutex<IrqController>>,
}

impl IrqSink {
    #[must_use]
    pub fn new(inner: Arc<Mutex<IrqController>>) -> Self {
        Self { inner }
    }

    pub fn raise(&self, irq: IrqId) {
        self.inner.lock().expect("irq").raise(irq);
    }
}

/// Sixteen consecutive IRQ lines packed in one IF/MK/PR word (`IF0`..`IF3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IrqGroup(u8);

impl IrqGroup {
    const COUNT: u8 = 4;

    fn from_index(index: u8) -> Option<Self> {
        (index < Self::COUNT).then_some(Self(index))
    }

    fn first_line(self) -> u8 {
        self.0.saturating_mul(16)
    }

    fn shift(self) -> u32 {
        u32::from(self.first_line())
    }
}

/// One 16-bit IF/MK/PR register. Window 0 is `IF0`/`MK0`/`PR00`…; window 1 is `IF2`….
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IfMkReg {
    If(IrqGroup),
    Mk(IrqGroup),
    /// `PRn0`: priority bit 0 of each line in the group.
    PrLow(IrqGroup),
    /// `PRn1`: priority bit 1 of each line in the group.
    PrHigh(IrqGroup),
}

impl IfMkReg {
    /// `window` 0 → `0xFFFE0`, 1 → `0xFFFD0`. `offset` is the even register address.
    fn at(window: u8, offset: u64) -> Option<Self> {
        if offset % 2 != 0 || offset >= 0x10 {
            return None;
        }
        let slot = (offset / 2) as u8;
        let group = IrqGroup::from_index(window.saturating_mul(2).saturating_add(slot % 2))?;
        Some(match slot / 2 {
            0 => Self::If(group),
            1 => Self::Mk(group),
            2 => Self::PrLow(group),
            3 => Self::PrHigh(group),
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeReg {
    Egp0,
    Egn0,
    Egp1,
    Egn1,
}

impl EdgeReg {
    fn at(offset: u64) -> Option<Self> {
        match offset {
            0 => Some(Self::Egp0),
            1 => Some(Self::Egn0),
            2 => Some(Self::Egp1),
            3 => Some(Self::Egn1),
            _ => None,
        }
    }

    fn rising(self) -> bool {
        matches!(self, Self::Egp0 | Self::Egp1)
    }

    fn pin_base(self) -> u8 {
        match self {
            Self::Egp0 | Self::Egn0 => 0,
            Self::Egp1 | Self::Egn1 => 8,
        }
    }
}

/// IF/MK/PR/EGP-EGN latch. Reset: IF=0, MK=all-1, PR=3.
pub struct IrqController {
    flag: u64,
    mask: u64,
    priority: [u8; IrqId::SLOTS],
    edge: [u8; IrqId::EXT_PINS],
    pending: Option<IrqRequest>,
}

impl IrqController {
    #[must_use]
    pub fn new() -> Self {
        Self {
            flag: 0,
            mask: 0,
            priority: [0; IrqId::SLOTS],
            edge: [0; IrqId::EXT_PINS],
            pending: None,
        }
    }

    pub fn raise(&mut self, irq: IrqId) {
        self.flag |= 1u64 << irq.index();
        self.recompute();
    }

    pub fn ack(&mut self, irq: IrqId) {
        self.flag &= !(1u64 << irq.index());
        self.recompute();
    }

    #[must_use]
    pub fn pending(&self) -> Option<IrqRequest> {
        self.pending
    }

    #[must_use]
    pub fn is_flag_set(&self, irq: IrqId) -> bool {
        self.flag & (1u64 << irq.index()) != 0
    }

    fn recompute(&mut self) {
        self.pending = None;
        let unmasked = self.flag & !self.mask;
        for priority in 0..IrqId::PRIORITY_LEVELS {
            let mut group = 0u64;
            for i in 0..IrqId::NUM {
                if self.priority[i as usize] == priority {
                    group |= 1u64 << i;
                }
            }
            let hits = unmasked & group;
            if hits != 0 {
                let index = IrqId(hits.trailing_zeros() as u8);
                self.pending = Some(IrqRequest { index, priority });
                drive_cpu(self.pending);
                return;
            }
        }
        drive_cpu(self.pending);
    }

    fn pack_lines(value: u64, group: IrqGroup) -> u16 {
        ((value >> group.shift()) & 0xFFFF) as u16
    }

    fn deposit_lines(dst: &mut u64, group: IrqGroup, width: u32, bit: u32, field: u16) {
        let shift = group.shift() + bit;
        let mask = if width >= 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        *dst = (*dst & !(mask << shift)) | ((u64::from(field) & mask) << shift);
    }

    fn pack_priority(&self, group: IrqGroup, high: bool) -> u16 {
        let mut bits = 0u16;
        for i in 0..16u8 {
            let line = group.first_line().saturating_add(i);
            if line >= IrqId::NUM {
                break;
            }
            let pr = self.priority[line as usize];
            let bit = if high { (pr >> 1) & 1 } else { pr & 1 };
            bits |= u16::from(bit) << i;
        }
        bits
    }

    fn unpack_priority(&mut self, group: IrqGroup, high: bool, value: u16, bit: u32, width: u32) {
        let start = bit as u8;
        let end = start.saturating_add(width as u8).min(16);
        for i in start..end {
            let line = group.first_line().saturating_add(i);
            if line >= IrqId::NUM {
                break;
            }
            let field_bit = ((value >> (i - start)) & 1) as u8;
            let slot = &mut self.priority[line as usize];
            if high {
                *slot = (*slot & !2) | (field_bit << 1);
            } else {
                *slot = (*slot & !1) | field_bit;
            }
        }
        self.recompute();
    }

    fn read_if_mk(&self, reg: IfMkReg) -> u16 {
        match reg {
            IfMkReg::If(g) => Self::pack_lines(self.flag, g),
            IfMkReg::Mk(g) => Self::pack_lines(self.mask, g),
            IfMkReg::PrLow(g) => self.pack_priority(g, false),
            IfMkReg::PrHigh(g) => self.pack_priority(g, true),
        }
    }

    fn write_if_mk_word(&mut self, reg: IfMkReg, value: u16) {
        match reg {
            IfMkReg::If(g) => {
                Self::deposit_lines(&mut self.flag, g, 16, 0, value);
                self.recompute();
            }
            IfMkReg::Mk(g) => {
                Self::deposit_lines(&mut self.mask, g, 16, 0, value);
                self.recompute();
            }
            IfMkReg::PrLow(g) => self.unpack_priority(g, false, value, 0, 16),
            IfMkReg::PrHigh(g) => self.unpack_priority(g, true, value, 0, 16),
        }
    }

    fn write_if_mk_byte(&mut self, reg: IfMkReg, high: bool, value: u8) {
        let bit = if high { 8 } else { 0 };
        match reg {
            IfMkReg::If(g) => {
                Self::deposit_lines(&mut self.flag, g, 8, bit, u16::from(value));
                self.recompute();
            }
            IfMkReg::Mk(g) => {
                Self::deposit_lines(&mut self.mask, g, 8, bit, u16::from(value));
                self.recompute();
            }
            IfMkReg::PrLow(g) => self.unpack_priority(g, false, u16::from(value), bit, 8),
            IfMkReg::PrHigh(g) => self.unpack_priority(g, true, u16::from(value), bit, 8),
        }
    }

    fn read_edge(&self, reg: EdgeReg) -> u8 {
        let mut bits = 0u8;
        for i in 0..8u8 {
            let pin = reg.pin_base().saturating_add(i);
            if pin as usize >= IrqId::EXT_PINS {
                break;
            }
            let mask = if reg.rising() { 0x02 } else { 0x01 };
            if self.edge[pin as usize] & mask != 0 {
                bits |= 1 << i;
            }
        }
        bits
    }

    fn write_edge(&mut self, reg: EdgeReg, value: u8) {
        for i in 0..8u8 {
            let pin = reg.pin_base().saturating_add(i);
            if pin as usize >= IrqId::EXT_PINS {
                break;
            }
            let field = (value >> i) & 1;
            let e = &mut self.edge[pin as usize];
            if reg.rising() {
                *e = (*e & !0x02) | (field << 1);
            } else {
                *e = (*e & !0x01) | field;
            }
        }
    }

    #[cfg(test)]
    fn unmask_all(&mut self) {
        self.mask = 0;
        self.recompute();
    }

    #[cfg(test)]
    fn set_priority(&mut self, irq: IrqId, priority: u8) {
        self.priority[irq.index() as usize] = priority;
        self.recompute();
    }
}

impl Default for IrqController {
    fn default() -> Self {
        let mut s = Self::new();
        Resettable::reset(&mut s);
        s
    }
}

impl Resettable for IrqController {
    fn reset(&mut self) {
        self.flag = 0;
        self.mask = u64::MAX;
        self.priority = [IrqId::PRIORITY_LEVELS - 1; IrqId::SLOTS];
        self.edge = [0; IrqId::EXT_PINS];
        self.recompute();
    }
}

fn oob(offset: u64, len: usize) -> BusError {
    BusError::OutOfRange {
        addr: offset,
        offset,
        len,
    }
}

pub struct IrqBankMmio {
    inner: Arc<Mutex<IrqController>>,
    window: u8,
}

impl IrqBankMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<IrqController>>, window: u8) -> Self {
        Self { inner, window }
    }
}

impl MemoryMapped for IrqBankMmio {
    fn len(&self) -> u64 {
        0x10
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        let g = self.inner.lock().expect("irq");
        match buf.len() {
            1 => {
                let aligned = offset & !1;
                let Some(reg) = IfMkReg::at(self.window, aligned) else {
                    return Err(oob(offset, 1));
                };
                let word = g.read_if_mk(reg);
                buf[0] = if offset & 1 == 0 {
                    word as u8
                } else {
                    (word >> 8) as u8
                };
                Ok(())
            }
            2 => {
                let Some(reg) = IfMkReg::at(self.window, offset) else {
                    return Err(oob(offset, 2));
                };
                buf.copy_from_slice(&g.read_if_mk(reg).to_le_bytes());
                Ok(())
            }
            len => Err(oob(offset, len)),
        }
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        let mut g = self.inner.lock().expect("irq");
        match buf.len() {
            1 => {
                let aligned = offset & !1;
                let Some(reg) = IfMkReg::at(self.window, aligned) else {
                    return Err(oob(offset, 1));
                };
                g.write_if_mk_byte(reg, offset & 1 != 0, buf[0]);
                Ok(())
            }
            2 => {
                let Some(reg) = IfMkReg::at(self.window, offset) else {
                    return Err(oob(offset, 2));
                };
                g.write_if_mk_word(reg, u16::from_le_bytes([buf[0], buf[1]]));
                Ok(())
            }
            len => Err(oob(offset, len)),
        }
    }

    fn host_ptr(&mut self) -> Option<*mut u8> {
        None
    }

    fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::NotLoadable { addr: offset })
    }
}

pub struct IrqEdgeMmio {
    inner: Arc<Mutex<IrqController>>,
}

impl IrqEdgeMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<IrqController>>) -> Self {
        Self { inner }
    }
}

impl MemoryMapped for IrqEdgeMmio {
    fn len(&self) -> u64 {
        4
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if buf.len() != 1 {
            return Err(oob(offset, buf.len()));
        }
        let Some(reg) = EdgeReg::at(offset) else {
            return Err(oob(offset, 1));
        };
        buf[0] = self.inner.lock().expect("irq").read_edge(reg);
        Ok(())
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        if buf.len() != 1 {
            return Err(oob(offset, buf.len()));
        }
        let Some(reg) = EdgeReg::at(offset) else {
            return Err(oob(offset, 1));
        };
        self.inner.lock().expect("irq").write_edge(reg, buf[0]);
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
    fn reset_masks_all_and_clears_if() {
        let irq = IrqController::default();
        assert!(!irq.is_flag_set(IrqId::INTTM00));
        assert!(irq.pending().is_none());
    }

    #[test]
    fn raise_stays_pending_only_when_unmasked() {
        let mut irq = IrqController::default();
        irq.raise(IrqId::INTTM00);
        assert!(irq.is_flag_set(IrqId::INTTM00));
        assert!(irq.pending().is_none());
        irq.write_if_mk_word(IfMkReg::Mk(IrqGroup(0)), 0);
        let pending = irq.pending().unwrap();
        assert_eq!(pending.index, IrqId::INTTM00);
        assert_eq!(pending.priority, 3);
    }

    #[test]
    fn lower_priority_number_wins() {
        let mut irq = IrqController::default();
        irq.unmask_all();
        irq.raise(IrqId::INTTM00);
        irq.raise(IrqId::INTST0);
        irq.set_priority(IrqId::INTTM00, 0);
        let pending = irq.pending().unwrap();
        assert_eq!(pending.index, IrqId::INTTM00);
        assert_eq!(pending.priority, 0);
    }

    #[test]
    fn ack_clears_flag() {
        let mut irq = IrqController::default();
        irq.unmask_all();
        irq.raise(IrqId::INTST0);
        irq.ack(IrqId::INTST0);
        assert!(!irq.is_flag_set(IrqId::INTST0));
        assert!(irq.pending().is_none());
    }

    #[test]
    fn word_write_updates_if0_in_one_deposit() {
        let irq = Arc::new(Mutex::new(IrqController::default()));
        let mut mmio = IrqBankMmio::new(Arc::clone(&irq), 0);
        mmio.write(0, &[0x34, 0x12]).unwrap();
        let mut buf = [0u8; 2];
        mmio.read(0, &mut buf).unwrap();
        assert_eq!(buf, [0x34, 0x12]);
        let g = irq.lock().unwrap();
        assert!(g.is_flag_set(IrqId::from_index(2).unwrap()));
        assert!(g.is_flag_set(IrqId::from_index(4).unwrap()));
        assert!(g.is_flag_set(IrqId::from_index(12).unwrap()));
        assert!(!g.is_flag_set(IrqId::INTTM00));
    }

    #[test]
    fn byte_write_touches_only_if0h() {
        let irq = Arc::new(Mutex::new(IrqController::default()));
        let mut mmio = IrqBankMmio::new(Arc::clone(&irq), 0);
        mmio.write(0, &[0xFF, 0x00]).unwrap();
        mmio.write(1, &[0x40]).unwrap();
        let mut buf = [0u8; 2];
        mmio.read(0, &mut buf).unwrap();
        assert_eq!(buf, [0xFF, 0x40]);
        assert!(irq.lock().unwrap().is_flag_set(IrqId::INTTM00));
    }
}
