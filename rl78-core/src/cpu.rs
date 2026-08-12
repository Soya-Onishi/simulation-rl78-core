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
use std::sync::{Mutex, Once, OnceLock};

use sim_kernel::{
    Addr, Breakpoint, BreakpointId, Cpu, MemoryBus, PendingStop, Quantum, RegId, SimError,
    resolve_after_tlib_execute,
};

use crate::callbacks::{self, set_io_handler};
use crate::ffi::{self, Rl78Reg};

/// Default fetch window (all `0x00` = RL78 NOP).
///
/// TODO(phase D): replace this stand-in with pointers into `MemoryBus`
/// `MappedRegion` devices (`Rom` / `Ram` `Vec<u8>`). `bind_memory` will walk
/// the bus, register each RAM/ROM window via `map_host_region` + `tlib_map_range`,
/// and mark MMIO pages (`MagicProbe`, unmapped SFR, …) with `tlib_set_page_io_accessed`
/// plus an `IoHandler` that forwards to `MemoryBus::read` / `write`. Until then
/// this buffer is **not** the machine map — only a fetchable window at guest
/// address 0 so `tlib_execute` / reset-vector reads do not abort.
const DEFAULT_CODE_WINDOW: usize = 4096;

static SEAT: Mutex<bool> = Mutex::new(false);
static TLIB_INIT: Once = Once::new();
struct CodeWindow(*mut u8);
// Safety: access is gated by `TlibSeat` (single owner).
unsafe impl Send for CodeWindow {}
unsafe impl Sync for CodeWindow {}

static CODE_WINDOW: OnceLock<CodeWindow> = OnceLock::new();
static CODE_MAPPED: AtomicBool = AtomicBool::new(false);

/// Unit-test only: after a CPU that never ran `tlib_execute` is dropped, the
/// next `Rl78Cpu::new` should call `tlib_reset` so leftover PC/regs do not leak
/// between tests. Production holds one CPU for the process lifetime and does
/// not use this flag. `tlib_reset` after a real `tlib_execute` can SIGSEGV
/// (stale TCG/TLB), so tests that executed guest code skip the next reset.
#[cfg(test)]
static UNIT_TEST_RESET_TLIB_ON_NEXT_NEW: AtomicBool = AtomicBool::new(true);

/// Process-wide tlib ownership token (`Send`, unlike `MutexGuard`).
///
/// Production never releases the seat until process exit (one `Rl78Cpu`).
/// A second `acquire` while the seat is held is a programming error.
/// Sequential unit tests drop the previous CPU first, which frees the seat.
struct TlibSeat;

impl TlibSeat {
    fn acquire() -> Self {
        let mut taken = SEAT.lock().unwrap_or_else(|e| e.into_inner());
        if *taken {
            drop(taken);
            panic!("Rl78Cpu: tlib seat already held (only one live instance is allowed)");
        }
        *taken = true;
        Self
    }
}

impl Drop for TlibSeat {
    fn drop(&mut self) {
        let mut taken = SEAT.lock().unwrap_or_else(|e| e.into_inner());
        *taken = false;
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
    // TODO(phase D): delete this helper. `bind_memory` will map `Rom`/`Ram`
    // backing stores from `MemoryBus` instead of this process-wide NOP window.
    //
    // Does not call `tlib_reset` — reset runs on each CPU instance in
    // [`Rl78Cpu::new`] (not on first map only; see Bugbot "CPU recreate skips
    // tlib reset").
    let ptr = code_window_ptr();
    unsafe {
        callbacks::map_host_region(0, DEFAULT_CODE_WINDOW as u64, ptr);
        if !CODE_MAPPED.swap(true, Ordering::AcqRel) {
            std::ptr::write_bytes(ptr, 0, DEFAULT_CODE_WINDOW);
            ffi::tlib_map_range(0, DEFAULT_CODE_WINDOW as u64);
        }
    }
}

/// RL78 CPU backed by tlib. At most one live instance may exist (tlib singleton).
pub struct Rl78Cpu {
    /// Held for the CPU lifetime. A second live `Rl78Cpu` panics in `TlibSeat::acquire`.
    _seat: TlibSeat,
    /// Callback-forced stops; see [`PendingStop`].
    pending: PendingStop,
    /// Kernel breakpoint table (id + addr). tlib only stores addresses, not
    /// [`BreakpointId`], so `EXCP_DEBUG` is mapped back to an id via PC lookup.
    breakpoints: Vec<Breakpoint>,
    /// Unit-test only: true after this instance ran `tlib_execute`. See
    /// [`UNIT_TEST_RESET_TLIB_ON_NEXT_NEW`].
    #[cfg(test)]
    unit_test_guest_executed: bool,
}

impl Rl78Cpu {
    /// Initialize (once per process) and take the tlib seat for the `rl78` CPU.
    ///
    /// Panics if another [`Rl78Cpu`] is still alive. tlib is not disposed between
    /// sequential test instances — only reset — because dispose/re-init is
    /// fragile once host callbacks are overridden.
    #[must_use]
    pub fn new() -> Self {
        let seat = TlibSeat::acquire();
        ensure_tlib_initialized();
        set_io_handler(None);
        ensure_default_memory_mapped();
        // TODO: if construction paths beyond `new` are added (e.g. `from_elf`,
        // reinit), consolidate `tlib_reset` and related CPU-state init into
        // `init_tlib_cpu_state()` and call it from every entry point instead of
        // duplicating reset logic here.
        #[cfg(test)]
        if UNIT_TEST_RESET_TLIB_ON_NEXT_NEW.swap(false, Ordering::AcqRel) {
            unsafe { ffi::tlib_reset() };
        }
        #[cfg(not(test))]
        unsafe {
            ffi::tlib_reset();
        }

        Self {
            _seat: seat,
            pending: PendingStop::new(),
            breakpoints: Vec::new(),
            #[cfg(test)]
            unit_test_guest_executed: false,
        }
    }

    /// Access the callback stop latch (MMIO CB installs reasons here).
    pub fn pending_stop_mut(&mut self) -> &mut PendingStop {
        &mut self.pending
    }

    /// Map a guest physical range into tlib and register the host pointer for
    /// TCG direct access.
    ///
    /// TODO(phase D): `Cpu::bind_memory` becomes the only production caller.
    /// Planned flow:
    /// 1. `Machine::new` builds an immutable `MemoryBus` (`Rom` / `Ram` /
    ///    `MagicProbe` as `MappedRegion`s) and calls `bind_memory`.
    /// 2. `bind_memory` walks those regions (downcast or a bus helper that
    ///    exposes RAM/ROM backing `*mut u8` + guest base/size).
    /// 3. RAM/ROM → this method (`map_host_region` + `tlib_map_range`) so TCG
    ///    uses the **same** `Vec<u8>` as `MemoryBus`.
    /// 4. MMIO (`MagicProbe`, unmapped SFR, …) → [`Self::set_io_page`] +
    ///    `IoHandler` forwarding to `MemoryBus::read` / `write`.
    ///
    /// # Safety
    /// `host` must remain valid for as long as the range stays mapped (the
    /// `Rom`/`Ram` allocation on the bus).
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

    fn breakpoint_id_at_pc(&self, pc: Addr) -> Option<BreakpointId> {
        self.breakpoints
            .iter()
            .find(|bp| bp.enabled && bp.addr == pc)
            .map(|bp| bp.id)
    }
}

fn list_tlib_breakpoints() -> Vec<Addr> {
    let count = unsafe { ffi::tlib_breakpoint_count() };
    let mut addrs = vec![0u64; count];
    let written = unsafe { ffi::tlib_list_breakpoints(addrs.as_mut_ptr(), count) };
    addrs.truncate(written);
    addrs
}

impl Default for Rl78Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Rl78Cpu {
    fn drop(&mut self) {
        for addr in list_tlib_breakpoints() {
            unsafe { ffi::tlib_remove_breakpoint(addr) };
        }
        #[cfg(test)]
        if !self.unit_test_guest_executed {
            UNIT_TEST_RESET_TLIB_ON_NEXT_NEW.store(true, Ordering::Release);
        }
        set_io_handler(None);
    }
}

impl Cpu for Rl78Cpu {
    fn bind_memory(&mut self, _bus: &mut MemoryBus) {
        // TODO(phase D): walk `bus` MappedRegions — RAM/ROM → `map_memory` with
        // the device `Vec<u8>` pointer; MMIO → `set_io_page` + IoHandler → bus.
    }

    fn run_quantum(&mut self, max_instructions: u32) -> Quantum {
        let exit = unsafe { ffi::tlib_execute(max_instructions) };
        let instructions = unsafe { ffi::tlib_get_executed_instructions() };
        #[cfg(test)]
        if instructions > 0 {
            self.unit_test_guest_executed = true;
        }
        let pc = self.pc();
        self.finish_tlib_quantum(instructions, exit, self.breakpoint_id_at_pc(pc))
    }

    /// Inspect API for the control plane (`Command::ReadReg`): future CLI peek
    /// and GDB RSP. Peripherals must not use this; they talk to tlib/SFR via
    /// memory callbacks, not `RegId`.
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

    /// Push the kernel breakpoint table into tlib.
    ///
    /// TODO: production call site is `Simulator` after `Command::AddBreakpoint`
    /// / `RemoveBreakpoint` (inspect path, stopped). M1 CLI does not expose
    /// those commands yet; GDB RSP will send the same `Command`s. Until then
    /// only unit tests call this directly.
    fn sync_breakpoints(&mut self, breakpoints: &[Breakpoint]) {
        self.breakpoints = breakpoints.to_vec();
        let desired: Vec<Addr> = breakpoints
            .iter()
            .filter(|bp| bp.enabled)
            .map(|bp| bp.addr)
            .collect();

        let current = list_tlib_breakpoints();
        for addr in &current {
            if !desired.contains(addr) {
                unsafe { ffi::tlib_remove_breakpoint(*addr) };
            }
        }

        for addr in &desired {
            if !current.contains(addr) {
                unsafe { ffi::tlib_add_breakpoint(*addr) };
            }
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
    fn sync_breakpoints_removes_stale_tlib_entries() {
        let mut cpu = Rl78Cpu::new();
        cpu.sync_breakpoints(&[
            Breakpoint {
                id: BreakpointId(1),
                addr: 0x100,
                enabled: true,
            },
            Breakpoint {
                id: BreakpointId(2),
                addr: 0x200,
                enabled: true,
            },
        ]);
        let mut registered = list_tlib_breakpoints();
        registered.sort_unstable();
        assert_eq!(registered, vec![0x100, 0x200]);

        cpu.sync_breakpoints(&[Breakpoint {
            id: BreakpointId(2),
            addr: 0x200,
            enabled: true,
        }]);
        assert_eq!(list_tlib_breakpoints(), vec![0x200]);
    }

    #[test]
    fn breakpoint_id_at_pc_matches_kernel_table() {
        let mut cpu = Rl78Cpu::new();
        cpu.sync_breakpoints(&[Breakpoint {
            id: BreakpointId(7),
            addr: 0x42,
            enabled: true,
        }]);
        cpu.set_pc(0x42);
        assert_eq!(cpu.breakpoint_id_at_pc(0x42), Some(BreakpointId(7)));
        assert_eq!(cpu.breakpoint_id_at_pc(0x43), None);
    }

    #[test]
    fn second_live_cpu_panics() {
        let _cpu = Rl78Cpu::new();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Rl78Cpu::new();
        }));
        assert!(panicked.is_err());
    }

    #[test]
    fn new_resets_cpu_state_between_instances() {
        let mut cpu = Rl78Cpu::new();
        cpu.set_pc(0x1234);
        drop(cpu);
        let cpu2 = Rl78Cpu::new();
        assert_ne!(cpu2.pc(), 0x1234);
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
