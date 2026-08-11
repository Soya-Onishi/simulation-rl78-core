//! RL78 CPU wrapper.
//!
//! Until tlib is linked, this is an idle stand-in that retires one nop per tick
//! so the kernel loop, CLI, and inspect APIs can be exercised.

use sim_kernel::{Addr, Cpu, MemoryBus, Quantum, RegId, SimError, Tick};

pub const REG_PC: RegId = RegId(0);
pub const REG_SP: RegId = RegId(1);
pub const REG_PSW: RegId = RegId(2);

/// Placeholder CPU. Phase C replaces the body with `tlib_init` / `tlib_execute`.
#[derive(Clone, Debug)]
pub struct Rl78Cpu {
    pc: Addr,
    sp: u64,
    psw: u64,
    /// General registers R0–R31 (4 banks × 8), kept so inspect has a surface.
    gpr: [u64; 32],
}

impl Default for Rl78Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Rl78Cpu {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pc: 0,
            sp: 0,
            psw: 0,
            gpr: [0; 32],
        }
    }
}

impl Cpu for Rl78Cpu {
    fn run_quantum(&mut self, _bus: &mut MemoryBus, max_ticks: Tick) -> Quantum {
        // Idle nops: time advances, no memory traffic. A later tlib backend
        // will call `tlib_execute(max_ticks)` and report retired instructions.
        self.pc = self.pc.wrapping_add(max_ticks.0);
        Quantum {
            ticks: max_ticks,
            stop: None,
        }
    }

    fn read_reg(&self, id: RegId) -> Result<u64, SimError> {
        match id {
            REG_PC => Ok(self.pc),
            REG_SP => Ok(self.sp),
            REG_PSW => Ok(self.psw),
            RegId(n) if (16..48).contains(&n) => Ok(self.gpr[(n - 16) as usize]),
            other => Err(SimError::UnknownRegister(other)),
        }
    }

    fn write_reg(&mut self, id: RegId, value: u64) -> Result<(), SimError> {
        match id {
            REG_PC => self.pc = value,
            REG_SP => self.sp = value,
            REG_PSW => self.psw = value,
            RegId(n) if (16..48).contains(&n) => self.gpr[(n - 16) as usize] = value,
            other => return Err(SimError::UnknownRegister(other)),
        }
        Ok(())
    }

    fn pc(&self) -> Addr {
        self.pc
    }

    fn set_pc(&mut self, pc: Addr) {
        self.pc = pc;
    }
}
