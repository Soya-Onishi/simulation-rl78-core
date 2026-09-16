//! Cluster node helpers (transitional wrapper around [`crate::board_runtime`]).

use sim_kernel::{
    Addr, Breakpoint, Cpu, EventCtl, FirmwareError, Machine, MemoryBus, Quantum, RegId, SimError,
};

use crate::board_runtime::{BoardError, BoardOptions, run_board};
use crate::control::HostStopReason;
use crate::ipc::{NodeControl, board_id_hash};

/// Re-export idle poll used by older call sites.
pub use crate::board_runtime::BOARD_IDLE_POLL as NODE_IDLE_POLL;

/// Node-side failures (alias of [`BoardError`] for compatibility).
pub type NodeError = BoardError;

/// Helper used by the binary for typed args.
pub type NodeOptions = BoardOptions;

/// Drive the node outer loop via the shared board framework.
///
/// Uses a no-op [`IdleCpu`] so `cluster-node` keeps working until
/// `rl78-minimal-board` replaces it. Virtual time comes from the simulator clock.
pub fn run_node(opts: NodeOptions) -> Result<(), NodeError> {
    run_board(opts, || {
        Machine::new(IdleCpu::default(), MemoryBus::new(), EventCtl::new())
    })
}

/// Notify the arbiter of a host-initiated stop when the reason is cluster-relevant.
///
/// Returns `Ok(true)` if [`crate::control::ControlToArbiter::HostStop`] was sent.
pub fn notify_host_stop(
    control: &NodeControl,
    board_id: &str,
    reason: &str,
) -> Result<bool, NodeError> {
    let Some(reason) = HostStopReason::from_label(reason) else {
        return Ok(false);
    };
    control.publish(&crate::control::ControlToArbiter::HostStop {
        from: board_id_hash(board_id),
        reason,
    })?;
    Ok(true)
}

/// Transitional CPU for `cluster-node` until a real board binary lands.
#[derive(Default)]
struct IdleCpu {
    pc: u64,
}

impl Cpu for IdleCpu {
    fn load_firmware(&mut self, _bus: &mut MemoryBus, _image: &[u8]) -> Result<(), FirmwareError> {
        Ok(())
    }

    fn run_quantum(&mut self, max_instructions: u32) -> Quantum {
        let n = u64::from(max_instructions);
        self.pc = self.pc.wrapping_add(n);
        Quantum {
            instructions: n,
            stop: None,
        }
    }

    fn read_reg(&self, id: RegId) -> Result<u64, SimError> {
        if id.0 == 0 {
            Ok(self.pc)
        } else {
            Err(SimError::UnknownRegister(id))
        }
    }

    fn write_reg(&mut self, id: RegId, value: u64) -> Result<(), SimError> {
        if id.0 == 0 {
            self.pc = value;
            Ok(())
        } else {
            Err(SimError::UnknownRegister(id))
        }
    }

    fn pc(&self) -> Addr {
        self.pc
    }

    fn set_pc(&mut self, pc: Addr) {
        self.pc = pc;
    }

    fn sync_breakpoints(&mut self, _breakpoints: &[Breakpoint]) {}
}
