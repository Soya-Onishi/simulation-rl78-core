//! RL78 architecture description for [`gdbstub`].

use core::num::NonZeroUsize;

use gdbstub::arch::{Arch, RegId, Registers};

/// Number of registers exposed in the `g`/`G` packets (matches [`crate::cpu::RegId`] 0..17).
pub const GDB_REG_COUNT: usize = 18;

const TARGET_XML: &str = r#"<?xml version="1.0"?>
<!DOCTYPE target SYSTEM "gdb-target.dtd">
<target version="1.0">
  <architecture>rl78</architecture>
  <feature name="org.gnu.gdb.rl78.core">
    <reg name="x" bitsize="32"/>
    <reg name="a" bitsize="32"/>
    <reg name="c" bitsize="32"/>
    <reg name="b" bitsize="32"/>
    <reg name="e" bitsize="32"/>
    <reg name="d" bitsize="32"/>
    <reg name="l" bitsize="32"/>
    <reg name="h" bitsize="32"/>
    <reg name="pc" bitsize="32" type="code_ptr"/>
    <reg name="sp" bitsize="32" type="data_ptr"/>
    <reg name="es" bitsize="32"/>
    <reg name="cs" bitsize="32"/>
    <reg name="psw_cy" bitsize="32"/>
    <reg name="psw_isp" bitsize="32"/>
    <reg name="psw_rbs" bitsize="32"/>
    <reg name="psw_ac" bitsize="32"/>
    <reg name="psw_z" bitsize="32"/>
    <reg name="psw_ie" bitsize="32"/>
  </feature>
</target>
"#;

/// Marker type for the RL78 GDB architecture.
pub enum Rl78Arch {}

/// Flattened register file used by the GDB `g`/`G` packets.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct Rl78Regs {
    /// Values for [`crate::cpu::RegId`] 0..17, each encoded as 32-bit LE.
    pub values: [u32; GDB_REG_COUNT],
}

impl Registers for Rl78Regs {
    type ProgramCounter = u64;

    fn pc(&self) -> Self::ProgramCounter {
        u64::from(self.values[8])
    }

    fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
        for value in &self.values {
            for b in value.to_le_bytes() {
                write_byte(Some(b));
            }
        }
    }

    fn gdb_deserialize(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if bytes.len() != GDB_REG_COUNT * 4 {
            return Err(());
        }
        for (i, chunk) in bytes.chunks_exact(4).enumerate() {
            self.values[i] = u32::from_le_bytes(chunk.try_into().map_err(|_| ())?);
        }
        Ok(())
    }
}

/// GDB register id (0..17).
#[derive(Debug, Copy, Clone)]
#[allow(dead_code)] // Read via RegId::from_raw_id / Debug; field kept for clarity.
pub struct Rl78RegId(pub usize);

impl RegId for Rl78RegId {
    fn from_raw_id(id: usize) -> Option<(Self, Option<NonZeroUsize>)> {
        if id < GDB_REG_COUNT {
            Some((Self(id), NonZeroUsize::new(4)))
        } else {
            None
        }
    }
}

impl Arch for Rl78Arch {
    type Usize = u64;
    type Registers = Rl78Regs;
    type RegId = Rl78RegId;
    type BreakpointKind = ();

    fn target_description_xml() -> Option<&'static str> {
        Some(TARGET_XML)
    }
}
