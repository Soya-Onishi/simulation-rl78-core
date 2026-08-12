//! RL78 CPU wrapper.
//!
//! Architectural register state lives in tlib once phase C links it. This type
//! is only an idle stand-in so the kernel loop and CLI can run before that.
//!
//! # Where `resolve_after_tlib_execute` runs
//!
//! [`Simulator::poll`][sim_kernel::Simulator] never calls it. Mapping tlib exit
//! codes / [`PendingStop`] into [`Quantum::stop`] happens inside this CPU's
//! [`Cpu::run_quantum`] via [`Rl78Cpu::finish_tlib_quantum`] (phase C will call
//! that right after `tlib_execute`).

use sim_kernel::{
    Addr, BreakpointId, Cpu, MemoryBus, PendingStop, Quantum, SimError, resolve_after_tlib_execute,
};

/// Placeholder CPU. Phase C replaces the body with `tlib_init` / `tlib_execute`
/// and routes [`Cpu::read_reg`] / [`Cpu::write_reg`] to tlib — do not keep a
/// parallel GPR bank here.
#[derive(Clone, Debug, Default)]
pub struct Rl78Cpu {
    /// Host-only PC for the idle nop loop. Discarded when tlib owns the CPU.
    stub_pc: Addr,
    /// Callback-forced stops; see [`PendingStop`].
    pending: PendingStop,
}

impl Rl78Cpu {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Access the callback stop latch (MMIO CB installs reasons here).
    pub fn pending_stop_mut(&mut self) -> &mut PendingStop {
        &mut self.pending
    }

    /// Production call site for tlib exit → [`Quantum`] (used after `tlib_execute`).
    ///
    /// Phase C `run_quantum` will look like:
    /// ```ignore
    /// let exit = unsafe { tlib_execute(max_instructions as i32) };
    /// let instructions = unsafe { tlib_get_executed_instructions() };
    /// self.finish_tlib_quantum(instructions, exit, self.breakpoint_id_at(pc))
    /// ```
    #[must_use]
    pub fn finish_tlib_quantum(
        &mut self,
        instructions: u64,
        tlib_exit: i32,
        breakpoint_at_pc: Option<BreakpointId>,
    ) -> Quantum {
        Quantum {
            instructions,
            stop: resolve_after_tlib_execute(&mut self.pending, tlib_exit, breakpoint_at_pc),
        }
    }
}

impl Cpu for Rl78Cpu {
    fn bind_memory(&mut self, _bus: &mut MemoryBus) {
        // Once at Machine::new: install bus for tlib memory callbacks.
    }

    fn run_quantum(&mut self, max_instructions: u64) -> Quantum {
        // Idle nops until tlib is linked. Phase C replaces this body with
        // tlib_execute + finish_tlib_quantum (not Simulator::poll).
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

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{StopReason, TlibExit};

    #[test]
    fn finish_tlib_quantum_maps_excp_debug() {
        let mut cpu = Rl78Cpu::new();
        let q = cpu.finish_tlib_quantum(4, TlibExit::Debug as i32, Some(BreakpointId(9)));
        assert_eq!(q.instructions, 4);
        assert_eq!(
            q.stop,
            Some(StopReason::Breakpoint {
                id: BreakpointId(9)
            })
        );
    }

    #[test]
    fn finish_tlib_quantum_prefers_pending_stop() {
        let mut cpu = Rl78Cpu::new();
        cpu.pending_stop_mut().request(StopReason::Unmapped {
            addr: 0x20,
            write: false,
        });
        let q = cpu.finish_tlib_quantum(1, TlibExit::ReturnRequest as i32, None);
        assert_eq!(
            q.stop,
            Some(StopReason::Unmapped {
                addr: 0x20,
                write: false
            })
        );
    }
}
