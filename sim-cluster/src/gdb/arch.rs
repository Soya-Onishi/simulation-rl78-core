//! GNU RL78 register blob for [`gdbstub`] (`g`/`G` packets).

use core::num::NonZeroUsize;

use gdbstub::arch::{Arch, RegId, Registers};

/// Matches GNU `rl78-tdep` raw `g` length.
pub const GDB_G_PACKET_LEN: usize = 43;

pub enum Rl78Arch {}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Rl78GdbRegs {
    pub raw: [u8; GDB_G_PACKET_LEN],
}

impl Default for Rl78GdbRegs {
    fn default() -> Self {
        Self {
            raw: [0; GDB_G_PACKET_LEN],
        }
    }
}

impl Registers for Rl78GdbRegs {
    type ProgramCounter = u32;

    fn pc(&self) -> Self::ProgramCounter {
        u32::from_le_bytes(self.raw[35..39].try_into().unwrap_or([0; 4]))
    }

    fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
        for b in self.raw {
            write_byte(Some(b));
        }
    }

    fn gdb_deserialize(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if bytes.len() != GDB_G_PACKET_LEN {
            return Err(());
        }
        self.raw.copy_from_slice(bytes);
        Ok(())
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Rl78RegId(#[allow(dead_code)] pub usize);

impl RegId for Rl78RegId {
    fn from_raw_id(id: usize) -> Option<(Self, Option<NonZeroUsize>)> {
        if id < GDB_G_PACKET_LEN {
            Some((Self(id), NonZeroUsize::new(1)))
        } else {
            None
        }
    }
}

impl Arch for Rl78Arch {
    type Usize = u32;
    type Registers = Rl78GdbRegs;
    type RegId = Rl78RegId;
    type BreakpointKind = ();

    fn target_description_xml() -> Option<&'static str> {
        None
    }
}
