//! [`gdbstub`] [`Target`] backed by [`SimControl`] / [`SimEvents`].

use std::collections::HashMap;
use std::time::Duration;

use gdbstub::common::Signal;
use gdbstub::target::ext::base::BaseOps;
use gdbstub::target::ext::base::singlethread::{
    SingleThreadBase, SingleThreadResume, SingleThreadSingleStep,
};
use gdbstub::target::ext::breakpoints::{
    Breakpoints, BreakpointsOps, SwBreakpoint, SwBreakpointOps,
};
use gdbstub::target::{Target, TargetError, TargetResult};

use crate::breakpoint::BreakpointId;
use crate::command::{Command, InspectResult, Response};
use crate::cpu::RegId;
use crate::sim::{SimControl, SimEvents};
use crate::stop::StopReason;

use super::arch::{GDB_REG_COUNT, Rl78Arch, Rl78Regs};

/// GDB target that drives the simulation through the control plane.
pub struct SimGdbTarget {
    ctrl: SimControl,
    events: SimEvents,
    breakpoints: HashMap<u64, BreakpointId>,
}

impl SimGdbTarget {
    pub fn new(ctrl: SimControl, events: SimEvents) -> Self {
        Self {
            ctrl,
            events,
            breakpoints: HashMap::new(),
        }
    }

    fn rpc(&mut self, cmd: Command) -> Result<Response, &'static str> {
        self.ctrl
            .send(cmd)
            .map_err(|_| "sim command channel closed")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(Response::Started | Response::Stopped(_)) => {}
                Ok(other) => return Ok(other),
                Err(_) => {}
            }
        }
        Err("timed out waiting for sim response")
    }

    pub(super) fn notify_halt(&mut self, reason: StopReason) {
        let _ = self.ctrl.send(Command::NotifyHalt {
            reason: reason.clone(),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(Response::Inspect(InspectResult::Ok) | Response::Error(_)) => return,
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }

    pub(super) fn request_stop(&mut self) -> Result<(), &'static str> {
        self.ctrl.stop().map_err(|_| "sim command channel closed")
    }

    pub(super) fn wait_stopped(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<StopReason>, &'static str> {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            match self
                .events
                .recv_timeout(remain.min(Duration::from_millis(20)))
            {
                Ok(Response::Stopped(reason)) => return Ok(Some(reason)),
                Ok(Response::Quit) => return Err("simulation quit"),
                Ok(_) => {}
                Err(_) => {}
            }
        }
        Ok(None)
    }

    pub(super) fn events_mut(&mut self) -> &mut SimEvents {
        &mut self.events
    }
}

impl Target for SimGdbTarget {
    type Arch = Rl78Arch;
    type Error = &'static str;

    #[inline(always)]
    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::SingleThread(self)
    }

    #[inline(always)]
    fn support_breakpoints(&mut self) -> Option<BreakpointsOps<'_, Self>> {
        Some(self)
    }

    /// Keep `g`/`m` payloads as plain hex so clients and tests stay simple.
    fn use_rle(&self) -> bool {
        false
    }
}

impl SingleThreadBase for SimGdbTarget {
    fn read_registers(&mut self, regs: &mut Rl78Regs) -> TargetResult<(), Self> {
        for id in 0..GDB_REG_COUNT as u32 {
            match self.rpc(Command::ReadReg { id: RegId(id) }) {
                Ok(Response::Inspect(InspectResult::Reg { value, .. })) => {
                    regs.values[id as usize] = value as u32;
                }
                Ok(_) | Err(_) => {
                    regs.values[id as usize] = 0;
                }
            }
        }
        Ok(())
    }

    fn write_registers(&mut self, regs: &Rl78Regs) -> TargetResult<(), Self> {
        for (id, value) in regs.values.iter().enumerate() {
            match self.rpc(Command::WriteReg {
                id: RegId(id as u32),
                value: u64::from(*value),
            }) {
                Ok(Response::Inspect(InspectResult::Ok)) => {}
                Ok(Response::Error(_)) => return Err(TargetError::NonFatal),
                Ok(_) | Err(_) => return Err(TargetError::Fatal("write register failed")),
            }
        }
        Ok(())
    }

    fn read_addrs(&mut self, start_addr: u64, data: &mut [u8]) -> TargetResult<usize, Self> {
        if data.len() > u32::MAX as usize {
            return Err(TargetError::NonFatal);
        }
        match self.rpc(Command::ReadMem {
            addr: start_addr,
            len: data.len() as u32,
        }) {
            Ok(Response::Inspect(InspectResult::Mem { data: mem, .. })) => {
                let n = mem.len().min(data.len());
                data[..n].copy_from_slice(&mem[..n]);
                Ok(n)
            }
            Ok(Response::Error(_)) => Err(TargetError::NonFatal),
            Ok(_) | Err(_) => Err(TargetError::Fatal("read memory failed")),
        }
    }

    fn write_addrs(&mut self, start_addr: u64, data: &[u8]) -> TargetResult<(), Self> {
        match self.rpc(Command::WriteMem {
            addr: start_addr,
            data: data.to_vec(),
        }) {
            Ok(Response::Inspect(InspectResult::Ok)) => Ok(()),
            Ok(Response::Error(_)) => Err(TargetError::NonFatal),
            Ok(_) | Err(_) => Err(TargetError::Fatal("write memory failed")),
        }
    }

    #[inline(always)]
    fn support_resume(
        &mut self,
    ) -> Option<gdbstub::target::ext::base::singlethread::SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadResume for SimGdbTarget {
    fn resume(&mut self, signal: Option<Signal>) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("continuing with a signal is not supported");
        }
        self.ctrl
            .start()
            .map_err(|_| "sim command channel closed")?;
        Ok(())
    }

    #[inline(always)]
    fn support_single_step(
        &mut self,
    ) -> Option<gdbstub::target::ext::base::singlethread::SingleThreadSingleStepOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadSingleStep for SimGdbTarget {
    fn step(&mut self, signal: Option<Signal>) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("stepping with a signal is not supported");
        }
        self.ctrl.step().map_err(|_| "sim command channel closed")?;
        Ok(())
    }
}

impl Breakpoints for SimGdbTarget {
    #[inline(always)]
    fn support_sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }
}

impl SwBreakpoint for SimGdbTarget {
    fn add_sw_breakpoint(&mut self, addr: u64, _kind: ()) -> TargetResult<bool, Self> {
        match self.rpc(Command::AddBreakpoint { addr }) {
            Ok(Response::Inspect(InspectResult::Breakpoint { id })) => {
                self.breakpoints.insert(addr, id);
                Ok(true)
            }
            Ok(Response::Error(_)) => Ok(false),
            Ok(_) | Err(_) => Err(TargetError::Fatal("add breakpoint failed")),
        }
    }

    fn remove_sw_breakpoint(&mut self, addr: u64, _kind: ()) -> TargetResult<bool, Self> {
        let Some(id) = self.breakpoints.remove(&addr) else {
            return Ok(false);
        };
        match self.rpc(Command::RemoveBreakpoint { id }) {
            Ok(Response::Inspect(InspectResult::Ok)) => Ok(true),
            Ok(Response::Error(_)) => Ok(false),
            Ok(_) | Err(_) => Err(TargetError::Fatal("remove breakpoint failed")),
        }
    }
}

pub(super) fn map_stop_reason(reason: &StopReason) -> gdbstub::stub::SingleThreadStopReason<u64> {
    use gdbstub::stub::SingleThreadStopReason;
    match reason {
        StopReason::Breakpoint { .. } => SingleThreadStopReason::SwBreak(()),
        StopReason::Step => SingleThreadStopReason::DoneStep,
        StopReason::ExternalStop => SingleThreadStopReason::Signal(Signal::SIGINT),
        StopReason::Halt => SingleThreadStopReason::Signal(Signal::SIGTRAP),
        StopReason::Unmapped { .. } => SingleThreadStopReason::Signal(Signal::SIGSEGV),
    }
}
