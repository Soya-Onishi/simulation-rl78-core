//! Test / mock CPU used only by `sim_tests`.

use std::collections::HashMap;

use crate::breakpoint::Breakpoint;
use crate::bus::{Addr, BusError, MemoryBus};
use crate::command::SimError;
use crate::cpu::{Cpu, Quantum, RegId};
use crate::stop::StopReason;

/// One scripted guest action. Each operation retires one instruction.
#[derive(Clone, Debug)]
pub enum ScriptOp {
    Nop,
    Write {
        addr: Addr,
        data: Vec<u8>,
    },
    /// Write that does not turn a bus error into a CPU stop (tests bus trap policy).
    WriteIgnoreError {
        addr: Addr,
        data: Vec<u8>,
    },
    Halt,
    SetPc(Addr),
}

/// Deterministic CPU for kernel unit tests.
#[derive(Debug, Default)]
pub struct ScriptedCpu {
    pc: Addr,
    regs: HashMap<u32, u64>,
    ops: Vec<ScriptOp>,
    idx: usize,
    breakpoints: Vec<Breakpoint>,
    /// Address of the bus bound for this quantum (`bind_memory`). Sim-thread only.
    bus_addr: usize,
}

impl ScriptedCpu {
    #[must_use]
    pub fn new(ops: Vec<ScriptOp>) -> Self {
        Self {
            pc: 0,
            regs: HashMap::new(),
            ops,
            idx: 0,
            breakpoints: Vec::new(),
            bus_addr: 0,
        }
    }

    #[must_use]
    pub fn nops(count: usize) -> Self {
        Self::new(vec![ScriptOp::Nop; count])
    }

    fn hit_breakpoint(&self) -> Option<StopReason> {
        self.breakpoints
            .iter()
            .find(|bp| bp.enabled && bp.addr == self.pc)
            .map(|bp| StopReason::Breakpoint { id: bp.id })
    }

    fn bus_mut(&mut self) -> &mut MemoryBus {
        assert!(
            self.bus_addr != 0,
            "bind_memory must be called before run_quantum"
        );
        unsafe { &mut *(self.bus_addr as *mut MemoryBus) }
    }
}

impl Cpu for ScriptedCpu {
    fn bind_memory(&mut self, bus: &mut MemoryBus) {
        self.bus_addr = bus as *mut MemoryBus as usize;
    }

    fn run_quantum(&mut self, max_instructions: u64) -> Quantum {
        let mut instructions = 0u64;
        while instructions < max_instructions {
            let op = self.ops.get(self.idx).cloned();
            self.idx = self.idx.saturating_add(1);
            instructions += 1;
            self.pc = self.pc.wrapping_add(1);
            match op {
                None | Some(ScriptOp::Halt) => {
                    return Quantum {
                        instructions,
                        stop: Some(StopReason::Halt),
                    };
                }
                Some(ScriptOp::Nop) => {}
                Some(ScriptOp::SetPc(pc)) => self.pc = pc,
                Some(ScriptOp::Write { addr, data }) => {
                    if let Err(err) = self.bus_mut().write(addr, &data) {
                        return unmapped_or_continue(instructions, err, true);
                    }
                }
                Some(ScriptOp::WriteIgnoreError { addr, data }) => {
                    let _ = self.bus_mut().write(addr, &data);
                }
            }
            if let Some(stop) = self.hit_breakpoint() {
                return Quantum {
                    instructions,
                    stop: Some(stop),
                };
            }
        }
        Quantum {
            instructions,
            stop: None,
        }
    }

    fn read_reg(&self, id: RegId) -> Result<u64, SimError> {
        if id.0 == 0 {
            return Ok(self.pc);
        }
        self.regs
            .get(&id.0)
            .copied()
            .ok_or(SimError::UnknownRegister(id))
    }

    fn write_reg(&mut self, id: RegId, value: u64) -> Result<(), SimError> {
        if id.0 == 0 {
            self.pc = value;
            return Ok(());
        }
        self.regs.insert(id.0, value);
        Ok(())
    }

    fn pc(&self) -> Addr {
        self.pc
    }

    fn set_pc(&mut self, pc: Addr) {
        self.pc = pc;
    }

    fn sync_breakpoints(&mut self, breakpoints: &[Breakpoint]) {
        self.breakpoints = breakpoints.to_vec();
    }
}

fn unmapped_or_continue(instructions: u64, err: BusError, write: bool) -> Quantum {
    match err {
        BusError::Unmapped { addr, .. } => Quantum {
            instructions,
            stop: Some(StopReason::Unmapped { addr, write }),
        },
        _ => Quantum {
            instructions,
            stop: None,
        },
    }
}
