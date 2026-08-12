//! RL78 CPU wrapper over statically linked tlib.
//!
//! # Where `resolve_after_tlib_execute` runs
//!
//! [`Simulator::poll`][sim_kernel::Simulator] never calls it. Mapping tlib exit
//! codes / [`PendingStop`] into [`Quantum::stop`] happens inside this CPU's
//! [`Cpu::run_quantum`] via [`Rl78Cpu::finish_tlib_quantum`] right after
//! `tlib_execute`.
//!
//! Memory callbacks live in [`crate::callbacks`]. Phase D will route IO pages
//! into [`MemoryBus`]; until then a small host-backed NOP window keeps
//! `run_quantum` safe for the CLI lifecycle tests.

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, Once, OnceLock};

use sim_kernel::{
    Addr, Breakpoint, BreakpointId, Cpu, MemoryBus, PendingStop, Quantum, RegId, SimError,
    resolve_after_tlib_execute,
};

use crate::callbacks::{self, set_io_handler};
use crate::ffi::{self, Rl78Reg};

/// Default fetch window (all `0x00` = RL78 NOP) until bus↔tlib wiring (phase D).
const DEFAULT_CODE_WINDOW: usize = 4096;

static SEAT: Mutex<bool> = Mutex::new(false);
static SEAT_CV: Condvar = Condvar::new();
static TLIB_INIT: Once = Once::new();
struct CodeWindow(*mut u8);
// Safety: access is gated by `TlibSeat` (single owner).
unsafe impl Send for CodeWindow {}
unsafe impl Sync for CodeWindow {}

static CODE_WINDOW: OnceLock<CodeWindow> = OnceLock::new();
static CODE_MAPPED: AtomicBool = AtomicBool::new(false);

/// Process-wide tlib ownership token (`Send`, unlike `MutexGuard`).
struct TlibSeat;

impl TlibSeat {
    fn acquire() -> Self {
        let mut taken = SEAT.lock().unwrap_or_else(|e| e.into_inner());
        while *taken {
            taken = SEAT_CV.wait(taken).unwrap_or_else(|e| e.into_inner());
        }
        *taken = true;
        Self
    }
}

impl Drop for TlibSeat {
    fn drop(&mut self) {
        let mut taken = SEAT.lock().unwrap_or_else(|e| e.into_inner());
        *taken = false;
        SEAT_CV.notify_one();
    }
}

fn ensure_tlib_initialized() {
    TLIB_INIT.call_once(|| {
        // Pull host_callbacks.o (strong tlib_* overrides) from its static archive.
        unsafe extern "C" {
            fn rl78_tlib_callbacks_anchor();
        }
        unsafe { rl78_tlib_callbacks_anchor() };

        let name = CString::new("rl78").expect("cpu name");
        let rc = unsafe { ffi::tlib_init(name.as_ptr().cast_mut()) };
        assert_eq!(rc, 0, "tlib_init failed ({rc})");
    });
}

fn code_window_ptr() -> *mut u8 {
    CODE_WINDOW
        .get_or_init(|| {
            let boxed = vec![0u8; DEFAULT_CODE_WINDOW].into_boxed_slice();
            CodeWindow(Box::into_raw(boxed) as *mut u8)
        })
        .0
}

fn ensure_default_memory_mapped() {
    if CODE_MAPPED.swap(true, Ordering::AcqRel) {
        return;
    }
    let ptr = code_window_ptr();
    unsafe {
        std::ptr::write_bytes(ptr, 0, DEFAULT_CODE_WINDOW);
        callbacks::map_host_region(0, DEFAULT_CODE_WINDOW as u64, ptr);
        ffi::tlib_map_range(0, DEFAULT_CODE_WINDOW as u64);
        ffi::tlib_reset();
    }
}

/// RL78 CPU backed by tlib. At most one live instance may exist (tlib singleton).
pub struct Rl78Cpu {
    /// Held for the CPU lifetime so a second `Rl78Cpu::new` waits instead of racing.
    _seat: TlibSeat,
    /// Callback-forced stops; see [`PendingStop`].
    pending: PendingStop,
}

impl Rl78Cpu {
    /// Initialize (once per process) and take the tlib seat for the `rl78` CPU.
    ///
    /// Blocks if another [`Rl78Cpu`] is still alive. tlib is not disposed between
    /// instances — only reset — because dispose/re-init is fragile once host
    /// callbacks are overridden.
    #[must_use]
    pub fn new() -> Self {
        let seat = TlibSeat::acquire();
        ensure_tlib_initialized();
        set_io_handler(None);
        ensure_default_memory_mapped();

        Self {
            _seat: seat,
            pending: PendingStop::new(),
        }
    }

    /// Access the callback stop latch (MMIO CB installs reasons here).
    pub fn pending_stop_mut(&mut self) -> &mut PendingStop {
        &mut self.pending
    }

    /// Map a guest physical range into tlib and register the host pointer for
    /// TCG direct access. Phase D will derive this from the bus map instead.
    ///
    /// # Safety
    /// `host` must remain valid for as long as the range stays mapped.
    pub unsafe fn map_memory(&mut self, guest_base: u64, length: u64, host: *mut u8) {
        unsafe {
            callbacks::map_host_region(guest_base, length, host);
            ffi::tlib_map_range(guest_base, length);
        }
    }

    /// Mark a page as IO-accessed so loads/stores call the host IO callbacks.
    pub fn set_io_page(&mut self, page_addr: u64) {
        unsafe { ffi::tlib_set_page_io_accessed(page_addr) };
    }

    /// Production call site for tlib exit → [`Quantum`] (used after `tlib_execute`).
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

    fn reg32(&self, reg: Rl78Reg) -> u32 {
        unsafe { ffi::tlib_get_register_value_32(reg as i32) }
    }

    fn set_reg32(&mut self, reg: Rl78Reg, value: u32) {
        unsafe { ffi::tlib_set_register_value_32(reg as i32, value) };
    }

    fn map_reg_id(id: RegId) -> Option<Rl78Reg> {
        match id.0 {
            0 => Some(Rl78Reg::X),
            1 => Some(Rl78Reg::A),
            2 => Some(Rl78Reg::C),
            3 => Some(Rl78Reg::B),
            4 => Some(Rl78Reg::E),
            5 => Some(Rl78Reg::D),
            6 => Some(Rl78Reg::L),
            7 => Some(Rl78Reg::H),
            8 => Some(Rl78Reg::Pc),
            9 => Some(Rl78Reg::Sp),
            10 => Some(Rl78Reg::Es),
            11 => Some(Rl78Reg::Cs),
            12 => Some(Rl78Reg::PswCy),
            13 => Some(Rl78Reg::PswIsp),
            14 => Some(Rl78Reg::PswRbs),
            15 => Some(Rl78Reg::PswAc),
            16 => Some(Rl78Reg::PswZ),
            17 => Some(Rl78Reg::PswIe),
            _ => None,
        }
    }
}

impl Default for Rl78Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Rl78Cpu {
    fn drop(&mut self) {
        // Leave host regions and the shared code window in place so TCG TLB/TB
        // entries remain valid until the next owner remaps. Clear only the IO
        // handler (which may hold borrowed Rust state).
        set_io_handler(None);
    }
}

impl Cpu for Rl78Cpu {
    fn bind_memory(&mut self, _bus: &mut MemoryBus) {
        // Phase D: walk the bus map, `tlib_map_range` / `set_io_page`, and install
        // an `IoHandler` that forwards to `MemoryBus`.
    }

    fn run_quantum(&mut self, max_instructions: u64) -> Quantum {
        let step = u32::try_from(max_instructions).unwrap_or(u32::MAX);
        let exit = unsafe { ffi::tlib_execute(step) };
        let instructions = unsafe { ffi::tlib_get_executed_instructions() };
        self.finish_tlib_quantum(instructions, exit, None)
    }

    fn read_reg(&self, id: RegId) -> Result<u64, SimError> {
        let reg = Self::map_reg_id(id).ok_or(SimError::UnknownRegister(id))?;
        Ok(u64::from(self.reg32(reg)))
    }

    fn write_reg(&mut self, id: RegId, value: u64) -> Result<(), SimError> {
        let reg = Self::map_reg_id(id).ok_or(SimError::UnknownRegister(id))?;
        self.set_reg32(reg, value as u32);
        Ok(())
    }

    fn pc(&self) -> Addr {
        u64::from(self.reg32(Rl78Reg::Pc))
    }

    fn set_pc(&mut self, pc: Addr) {
        self.set_reg32(Rl78Reg::Pc, pc as u32);
    }

    fn sync_breakpoints(&mut self, breakpoints: &[Breakpoint]) {
        for bp in breakpoints {
            unsafe { ffi::tlib_add_breakpoint(bp.addr) };
        }
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

    #[test]
    fn tlib_init_reports_rl78_arch() {
        let _cpu = Rl78Cpu::new();
        let arch = unsafe { std::ffi::CStr::from_ptr(ffi::tlib_get_arch()) };
        assert_eq!(arch.to_string_lossy(), "rl78");
    }

    #[test]
    fn tlib_execute_runs_default_nop_window() {
        let mut cpu = Rl78Cpu::new();
        let q = cpu.run_quantum(16);
        assert!(q.instructions > 0, "expected guest instructions, got {q:?}");
        assert!(q.stop.is_none());
    }
}

#[cfg(test)]
mod map_probe {
    use super::*;
    #[test]
    fn map_range_is_visible() {
        let mut cpu = Rl78Cpu::new();
        let mapped = unsafe { ffi::tlib_is_range_mapped(0, 1) };
        assert_ne!(mapped, 0, "expected range 0 mapped after Rl78Cpu::new");
        let p = crate::callbacks::rl78_host_guest_offset_to_host_ptr(0);
        assert!(!p.is_null(), "host ptr for 0");
        let _ = &mut cpu;
    }
}
