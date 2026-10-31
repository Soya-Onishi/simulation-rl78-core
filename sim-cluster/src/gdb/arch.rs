//! Architecture-neutral gdbstub [`Arch`]: an opaque `g` blob plus a PC.
//!
//! Packet layout, `target.xml`, and GNU register numbers belong in the CPU
//! crate (`hw/cpus/<arch>`). This stub only forwards bytes and uses 32-bit
//! RSP addresses (`Arch::Usize = u32`).

use core::num::NonZeroUsize;

use gdbstub::arch::{Arch, RegId, Registers};

/// Remote protocol architecture used by [`crate::gdb::target::ClusterGdbTarget`].
pub enum GdbArch {}

/// Opaque GDB `g`/`G` payload. `pc` is supplied by inspect, not parsed from `raw`.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct GdbRegs {
    pub raw: Vec<u8>,
    pub pc: u32,
}

impl Registers for GdbRegs {
    type ProgramCounter = u32;

    fn pc(&self) -> Self::ProgramCounter {
        self.pc
    }

    fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
        for byte in &self.raw {
            write_byte(Some(*byte));
        }
    }

    fn gdb_deserialize(&mut self, bytes: &[u8]) -> Result<(), ()> {
        self.raw.clear();
        self.raw.extend_from_slice(bytes);
        Ok(())
    }
}

/// Placeholder for a future `p`/`P` map (see issue #92). Unused while
/// single-register access is not implemented.
#[derive(Debug, Copy, Clone)]
pub struct GdbRegId(#[allow(dead_code)] pub usize);

impl RegId for GdbRegId {
    fn from_raw_id(id: usize) -> Option<(Self, Option<NonZeroUsize>)> {
        Some((Self(id), None))
    }
}

impl Arch for GdbArch {
    type Usize = u32;
    type Registers = GdbRegs;
    type RegId = GdbRegId;
    type BreakpointKind = ();

    fn target_description_xml() -> Option<&'static str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_round_trips_any_length() {
        let mut regs = GdbRegs {
            raw: vec![0x11, 0x22, 0x33],
            pc: 0x1234,
        };
        let mut out = Vec::new();
        regs.gdb_serialize(|b| out.push(b.unwrap()));
        assert_eq!(out, vec![0x11, 0x22, 0x33]);
        assert_eq!(regs.pc(), 0x1234);

        regs.gdb_deserialize(&[0xaa, 0xbb]).unwrap();
        assert_eq!(regs.raw, vec![0xaa, 0xbb]);
        assert_eq!(regs.pc(), 0x1234);
    }
}
