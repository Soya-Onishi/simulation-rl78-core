//! Test / mock CPU used only by `sim_tests`.

use std::collections::HashMap;

use crate::breakpoint::Breakpoint;
use crate::bus::{Addr, BusError, MemoryBus};
use crate::clock::Tick;
use crate::command::SimError;
use crate::cpu::{Cpu, Quantum, RegId};
use crate::stop::StopReason;

/// One scripted guest action. Each operation consumes one tick.
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
#[derive(Clone, Debug, Default)]
pub struct ScriptedCpu {
    pc: Addr,
    regs: HashMap<u32, u64>,
    ops: Vec<ScriptOp>,
    idx: usize,
    breakpoints: Vec<Breakpoint>,
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
}

impl Cpu for ScriptedCpu {
    fn run_quantum(&mut self, bus: &mut MemoryBus, max_ticks: Tick) -> Quantum {
        let mut ticks = Tick::ZERO;
        while ticks < max_ticks {
            let op = self.ops.get(self.idx).cloned();
            self.idx = self.idx.saturating_add(1);
            ticks += Tick(1);
            self.pc = self.pc.wrapping_add(1);
            match op {
                None | Some(ScriptOp::Halt) => {
                    return Quantum {
                        ticks,
                        stop: Some(StopReason::Halt),
                    };
                }
                Some(ScriptOp::Nop) => {}
                Some(ScriptOp::SetPc(pc)) => self.pc = pc,
                Some(ScriptOp::Write { addr, data }) => {
                    if let Err(err) = bus.write(addr, &data) {
                        return unmapped_or_continue(ticks, err, true);
                    }
                }
                Some(ScriptOp::WriteIgnoreError { addr, data }) => {
                    let _ = bus.write(addr, &data);
                }
            }
            if let Some(stop) = self.hit_breakpoint() {
                return Quantum {
                    ticks,
                    stop: Some(stop),
                };
            }
        }
        Quantum { ticks, stop: None }
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

fn unmapped_or_continue(ticks: Tick, err: BusError, write: bool) -> Quantum {
    match err {
        BusError::Unmapped { addr, .. } => Quantum {
            ticks,
            stop: Some(StopReason::Unmapped { addr, write }),
        },
        _ => Quantum { ticks, stop: None },
    }
}
