//! RL78 CPU wrapper.
//!
//! Architectural register state lives in tlib once phase C links it. This type
//! is only an idle stand-in so the kernel loop and CLI can run before that.

use sim_kernel::{Addr, Cpu, MemoryBus, Quantum, SimError, Tick};

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
    fn run_quantum(&mut self, _bus: &mut MemoryBus, max_ticks: Tick) -> Quantum {
        // Idle nops: time advances, no memory traffic. A later tlib backend
        // will call `tlib_execute(max_ticks)` and report retired instructions.
        self.stub_pc = self.stub_pc.wrapping_add(max_ticks.0);
        Quantum {
            ticks: max_ticks,
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
