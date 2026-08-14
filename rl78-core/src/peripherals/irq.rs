//! Interrupt controller (QEMU `hw/rl78/irq.c`).
//!
//! Absolute SFR bases are applied by the SoC map. CPU injection (`tlib_set_rl78_irq`)
//! is not wired here; [`IrqController::pending`] exposes the selected request.

use std::sync::{Arc, Mutex};

use sim_kernel::{BusError, MemoryMapped, Resettable};

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
                return;
            }
        }
    }

    fn deposit_flag(&mut self, bit: u8, width: u8, value: u16) {
        self.flag = deposit64(self.flag, bit, width, u64::from(value));
        self.recompute();
    }

    fn deposit_mask(&mut self, bit: u8, width: u8, value: u16) {
        self.mask = deposit64(self.mask, bit, width, u64::from(value));
        self.recompute();
    }

    fn write_priority(&mut self, head: u8, value: u16, high: bool) {
        for i in 0..16u8 {
            let index = head.saturating_mul(16).saturating_add(i);
            if index >= IrqId::NUM {
                break;
            }
            let bit = ((value >> i) & 1) as u8;
            let slot = &mut self.priority[index as usize];
            if high {
                *slot = (*slot & !2) | (bit << 1);
            } else {
                *slot = (*slot & !1) | bit;
            }
        }
        self.recompute();
    }

    fn read_priority(&self, head: u8, high: bool) -> u16 {
        let mut bits = 0u16;
        for i in 0..16u8 {
            let index = head.saturating_mul(16).saturating_add(i);
            if index >= IrqId::NUM {
                break;
            }
            let pr = self.priority[index as usize];
            let bit = if high { (pr >> 1) & 1 } else { pr & 1 };
            bits |= u16::from(bit) << i;
        }
        bits
    }

    fn read_bank(&self, bank: u8, offset: u64) -> u8 {
        let (kind, head, lo) = decode_bank(bank, offset);
        let word = match kind {
            BankReg::If => ((self.flag >> (head * 16)) & 0xFFFF) as u16,
            BankReg::Mk => ((self.mask >> (head * 16)) & 0xFFFF) as u16,
            BankReg::Pr0 => self.read_priority(head, false),
            BankReg::Pr1 => self.read_priority(head, true),
        };
        if lo { word as u8 } else { (word >> 8) as u8 }
    }

    fn write_bank(&mut self, bank: u8, offset: u64, value: u8) {
        let (kind, head, lo) = decode_bank(bank, offset);
        let bit = head * 16 + u8::from(!lo) * 8;
        match kind {
            BankReg::If => self.deposit_flag(bit, 8, u16::from(value)),
            BankReg::Mk => self.deposit_mask(bit, 8, u16::from(value)),
            BankReg::Pr0 => {
                let mut word = self.read_priority(head, false);
                word = deposit16(word, if lo { 0 } else { 8 }, 8, u16::from(value));
                self.write_priority(head, word, false);
            }
            BankReg::Pr1 => {
                let mut word = self.read_priority(head, true);
                word = deposit16(word, if lo { 0 } else { 8 }, 8, u16::from(value));
                self.write_priority(head, word, true);
            }
        }
    }

    fn read_edge(&self, offset: u64) -> u8 {
        let (egp, group) = match offset {
            0 => (true, 0u8),
            1 => (false, 0u8),
            2 => (true, 1u8),
            3 => (false, 1u8),
            _ => return 0,
        };
        let mut bits = 0u8;
        for i in 0..8u8 {
            let pin = group * 8 + i;
            if pin as usize >= IrqId::EXT_PINS {
                break;
            }
            let mask = if egp { 0x02 } else { 0x01 };
            if self.edge[pin as usize] & mask != 0 {
                bits |= 1 << i;
            }
        }
        bits
    }

    fn write_edge(&mut self, offset: u64, value: u8) {
        let (egp, group) = match offset {
            0 => (true, 0u8),
            1 => (false, 0u8),
            2 => (true, 1u8),
            3 => (false, 1u8),
            _ => return,
        };
        for i in 0..8u8 {
            let pin = group * 8 + i;
            if pin as usize >= IrqId::EXT_PINS {
                break;
            }
            let bit = (value >> i) & 1;
            let e = &mut self.edge[pin as usize];
            if egp {
                *e = (*e & !0x02) | (bit << 1);
            } else {
                *e = (*e & !0x01) | bit;
            }
        }
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

#[derive(Clone, Copy)]
enum BankReg {
    If,
    Mk,
    Pr0,
    Pr1,
}

fn decode_bank(bank: u8, offset: u64) -> (BankReg, u8, bool) {
    let word = (offset / 2) as u8;
    let lo = offset % 2 == 0;
    let group = word % 2;
    let head = bank * 2 + group;
    let kind = match word / 2 {
        0 => BankReg::If,
        1 => BankReg::Mk,
        2 => BankReg::Pr0,
        _ => BankReg::Pr1,
    };
    (kind, head, lo)
}

fn deposit64(val: u64, bit: u8, width: u8, field: u64) -> u64 {
    let mask = if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    (val & !(mask << bit)) | ((field & mask) << bit)
}

fn deposit16(val: u16, bit: u8, width: u8, field: u16) -> u16 {
    let mask = (1u16 << width) - 1;
    (val & !(mask << bit)) | ((field & mask) << bit)
}

fn oob(offset: u64, len: usize) -> BusError {
    BusError::OutOfRange {
        addr: offset,
        offset,
        len,
    }
}

fn rw_bytes(
    offset: u64,
    buf_len: usize,
    size: u64,
    mut each: impl FnMut(u64),
) -> Result<(), BusError> {
    match buf_len {
        1 if offset < size => {
            each(offset);
            Ok(())
        }
        2 if offset.saturating_add(2) <= size => {
            each(offset);
            each(offset + 1);
            Ok(())
        }
        len => Err(oob(offset, buf_len.max(len))),
    }
}

pub struct IrqBankMmio {
    inner: Arc<Mutex<IrqController>>,
    bank: u8,
}

impl IrqBankMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<IrqController>>, bank: u8) -> Self {
        Self { inner, bank }
    }
}

impl MemoryMapped for IrqBankMmio {
    fn len(&self) -> u64 {
        0x10
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        let g = self.inner.lock().expect("irq");
        let bank = self.bank;
        let mut i = 0;
        rw_bytes(offset, buf.len(), 0x10, |o| {
            buf[i] = g.read_bank(bank, o);
            i += 1;
        })
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        let mut g = self.inner.lock().expect("irq");
        let bank = self.bank;
        let mut i = 0;
        rw_bytes(offset, buf.len(), 0x10, |o| {
            g.write_bank(bank, o, buf[i]);
            i += 1;
        })
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
        let g = self.inner.lock().expect("irq");
        let mut i = 0;
        rw_bytes(offset, buf.len(), 4, |o| {
            buf[i] = g.read_edge(o);
            i += 1;
        })
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        let mut g = self.inner.lock().expect("irq");
        let mut i = 0;
        rw_bytes(offset, buf.len(), 4, |o| {
            g.write_edge(o, buf[i]);
            i += 1;
        })
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
        irq.deposit_mask(0, 16, 0);
        let pending = irq.pending().unwrap();
        assert_eq!(pending.index, IrqId::INTTM00);
        assert_eq!(pending.priority, 3);
    }

    #[test]
    fn lower_priority_number_wins() {
        let mut irq = IrqController::default();
        irq.deposit_mask(0, 64, 0);
        irq.raise(IrqId::INTTM00);
        irq.raise(IrqId::INTST0);
        let keep = !(1u16 << IrqId::INTTM00.index());
        irq.write_priority(0, keep, false);
        irq.write_priority(0, keep, true);
        let pending = irq.pending().unwrap();
        assert_eq!(pending.index, IrqId::INTTM00);
        assert_eq!(pending.priority, 0);
    }

    #[test]
    fn ack_clears_flag() {
        let mut irq = IrqController::default();
        irq.deposit_mask(0, 64, 0);
        irq.raise(IrqId::INTST0);
        irq.ack(IrqId::INTST0);
        assert!(!irq.is_flag_set(IrqId::INTST0));
        assert!(irq.pending().is_none());
    }
}
