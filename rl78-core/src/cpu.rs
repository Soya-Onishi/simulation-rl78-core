//! RL78 CPU wrapper.
//!
//! Architectural register state lives in tlib once phase C links it. This type
//! is only an idle stand-in so the kernel loop and CLI can run before that.

use sim_kernel::{Addr, Cpu, MemoryBus, Quantum, SimError};

/// Placeholder CPU. Phase C replaces the body with `tlib_init` / `tlib_execute`
/// and routes [`Cpu::read_reg`] / [`Cpu::write_reg`] to tlib — do not keep a
/// parallel GPR bank here.
#[derive(Clone, Debug, Default)]
pub struct Rl78Cpu {
    /// Host-only PC for the idle nop loop. Discarded when tlib owns the CPU.
    stub_pc: Addr,
}

impl Rl78Cpu {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Cpu for Rl78Cpu {
    fn bind_memory(&mut self, _bus: &mut MemoryBus) {
        // Phase C: install bus for tlib memory callbacks around `tlib_execute`.
    }

    fn run_quantum(&mut self, max_instructions: u64) -> Quantum {
        // Idle nops. A later tlib backend calls `tlib_execute(max_instructions)`
        // and reports retired instructions / pending stop from breakpoints.
        self.stub_pc = self.stub_pc.wrapping_add(max_instructions);
        Quantum {
            instructions: max_instructions,
            stop: None,
        }
    }

    fn read_reg(&self, id: sim_kernel::RegId) -> Result<u64, SimError> {
        Err(SimError::UnknownRegister(id))
    }

    fn write_reg(&mut self, id: sim_kernel::RegId, _value: u64) -> Result<(), SimError> {
        Err(SimError::UnknownRegister(id))
    }

    fn pc(&self) -> Addr {
        self.stub_pc
    }

    fn set_pc(&mut self, pc: Addr) {
        self.stub_pc = pc;
    }
}
