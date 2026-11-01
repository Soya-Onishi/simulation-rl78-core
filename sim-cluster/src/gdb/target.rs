//! [`gdbstub`] target that talks only through [`SimulationBackend`].

use std::sync::Arc;
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

use crate::backend::SimulationBackend;
use crate::inspect::{InspectReply, InspectRequest, MEM_CHUNK};

use super::arch::{GdbArch, GdbRegs};

pub struct ClusterGdbTarget {
    backend: Arc<dyn SimulationBackend>,
    sim_id: String,
    board_id: String,
}

impl ClusterGdbTarget {
    pub fn new(backend: Arc<dyn SimulationBackend>, sim_id: String, board_id: String) -> Self {
        Self {
            backend,
            sim_id,
            board_id,
        }
    }

    fn rpc(&self, req: InspectRequest) -> Result<InspectReply, String> {
        self.backend
            .inspect(&self.sim_id, &self.board_id, req)
            .map_err(|err| err.message)
    }

    pub(super) fn stop(&self) -> Result<(), String> {
        self.backend.stop(&self.sim_id).map_err(|err| err.message)
    }

    fn snapshot_status(&self) -> Result<crate::contract::StatusEvent, String> {
        self.backend.status(&self.sim_id).map_err(|err| err.message)
    }

    pub fn status_stop_reason(&self) -> Result<Option<String>, String> {
        let status = self.snapshot_status()?;
        if status.state == "running" {
            Ok(None)
        } else {
            Ok(status.stop_reason)
        }
    }

    fn wait_running(&self) -> Result<(), String> {
        let before = self.snapshot_status()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let now = self.snapshot_status()?;
            if now.state == "running"
                || now.virtual_time_ns != before.virtual_time_ns
                || now.stop_reason != before.stop_reason
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    /// Poll until the cluster is not `running`, or fail after 5 seconds.
    pub(super) fn wait_stopped(&self) -> Result<(), String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let now = self.snapshot_status()?;
            if now.state != "running" {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err("stop timeout".into())
    }

    pub fn clear_session_breakpoints(&self) -> Result<(), String> {
        match self.rpc(InspectRequest::ClearBreakpoints)? {
            InspectReply::Ok => Ok(()),
            InspectReply::Error { message } => Err(message),
            other => Err(format!("unexpected {other:?}")),
        }
    }
}

impl Target for ClusterGdbTarget {
    type Arch = GdbArch;
    type Error = String;

    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::SingleThread(self)
    }

    fn support_breakpoints(&mut self) -> Option<BreakpointsOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadBase for ClusterGdbTarget {
    fn read_registers(&mut self, regs: &mut GdbRegs) -> TargetResult<(), Self> {
        match self.rpc(InspectRequest::ReadRegs) {
            Ok(InspectReply::Regs { data, pc }) => {
                regs.raw = decode_hex(&data).map_err(|_| TargetError::NonFatal)?;
                regs.pc = pc as u32;
                Ok(())
            }
            Ok(_) | Err(_) => Err(TargetError::NonFatal),
        }
    }

    fn write_registers(&mut self, regs: &GdbRegs) -> TargetResult<(), Self> {
        match self.rpc(InspectRequest::WriteRegs {
            data: encode_hex(&regs.raw),
        }) {
            Ok(InspectReply::Ok) => Ok(()),
            Ok(_) | Err(_) => Err(TargetError::NonFatal),
        }
    }

    fn read_addrs(&mut self, start_addr: u32, data: &mut [u8]) -> TargetResult<usize, Self> {
        let mut filled = 0;
        while filled < data.len() {
            let chunk = (data.len() - filled).min(MEM_CHUNK as usize) as u32;
            match self.rpc(InspectRequest::ReadMem {
                addr: u64::from(start_addr) + filled as u64,
                len: chunk,
            }) {
                Ok(InspectReply::Mem { data: hex }) => {
                    let bytes = decode_hex(&hex).map_err(|_| TargetError::NonFatal)?;
                    let n = bytes.len().min(data.len() - filled);
                    data[filled..filled + n].copy_from_slice(&bytes[..n]);
                    filled += n;
                    if n == 0 {
                        break;
                    }
                }
                Ok(_) | Err(_) => return Err(TargetError::NonFatal),
            }
        }
        Ok(filled)
    }

    fn write_addrs(&mut self, start_addr: u32, data: &[u8]) -> TargetResult<(), Self> {
        let mut off = 0;

        for chunk in data.chunks(MEM_CHUNK as usize) {
            match self.rpc(InspectRequest::WriteMem {
                addr: u64::from(start_addr) + off as u64,
                data: encode_hex(chunk),
            }) {
                Ok(InspectReply::Ok) => off += chunk.len(),
                Ok(_) | Err(_) => return Err(TargetError::NonFatal),
            }
        }
        Ok(())
    }

    fn support_resume(
        &mut self,
    ) -> Option<gdbstub::target::ext::base::singlethread::SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadResume for ClusterGdbTarget {
    fn resume(&mut self, signal: Option<Signal>) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("continuing with a signal is not supported".into());
        }
        self.backend
            .start(&self.sim_id, None)
            .map_err(|err| err.message)?;
        self.wait_running()
    }

    fn support_single_step(
        &mut self,
    ) -> Option<gdbstub::target::ext::base::singlethread::SingleThreadSingleStepOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadSingleStep for ClusterGdbTarget {
    fn step(&mut self, signal: Option<Signal>) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("stepping with a signal is not supported".into());
        }
        self.backend
            .step(&self.sim_id, &self.board_id)
            .map_err(|err| err.message)?;
        self.wait_running()
    }
}

impl Breakpoints for ClusterGdbTarget {
    fn support_sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }
}

impl SwBreakpoint for ClusterGdbTarget {
    fn add_sw_breakpoint(&mut self, addr: u32, _kind: ()) -> TargetResult<bool, Self> {
        match self.rpc(InspectRequest::AddBreakpoint {
            addr: u64::from(addr),
        }) {
            Ok(InspectReply::Breakpoint { .. }) => Ok(true),
            Ok(InspectReply::Error { .. }) => Ok(false),
            Ok(_) | Err(_) => Err(TargetError::NonFatal),
        }
    }

    fn remove_sw_breakpoint(&mut self, addr: u32, _kind: ()) -> TargetResult<bool, Self> {
        match self.rpc(InspectRequest::RemoveBreakpoint {
            addr: u64::from(addr),
        }) {
            Ok(InspectReply::Ok) => Ok(true),
            Ok(InspectReply::Error { .. }) => Ok(false),
            Ok(_) | Err(_) => Err(TargetError::NonFatal),
        }
    }
}

pub fn map_status_reason(reason: Option<&str>) -> gdbstub::stub::SingleThreadStopReason<u32> {
    use gdbstub::stub::SingleThreadStopReason;
    match reason {
        Some("breakpoint") => SingleThreadStopReason::SwBreak(()),
        Some("step") => SingleThreadStopReason::DoneStep,
        Some("unmapped") => SingleThreadStopReason::Signal(Signal::SIGSEGV),
        _ => SingleThreadStopReason::Signal(Signal::SIGINT),
    }
}

fn decode_hex(text: &str) -> Result<Vec<u8>, ()> {
    if text.len() % 2 != 0 {
        return Err(());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
