//! RL78 CPU wrapper over statically linked tlib.
//!
//! # Where `resolve_after_tlib_execute` runs
//!
//! [`Simulator::poll`][sim_kernel::Simulator] never calls it. Mapping tlib exit
//! codes / [`PendingStop`] into [`Quantum::stop`] happens inside this CPU's
//! [`Cpu::run_quantum`] via [`Rl78Cpu::finish_tlib_quantum`] right after
//! `tlib_execute`.
//!
//! Memory callbacks live in [`crate::callbacks`]. [`Cpu::bind_memory`] maps
//! `Rom`/`Ram` host buffers into tlib and routes MMIO pages through the bound
//! [`MemoryBus`] (via [`callbacks::set_io_bus`]).

use std::ffi::CString;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

use sim_kernel::{
    Addr, Breakpoint, BreakpointId, Cpu, MemoryBus, PendingStop, Quantum, RegId, SimError,
    resolve_after_tlib_execute,
};

use crate::callbacks::{self, TLIB_PAGE_SIZE, clear_io_bus, set_io_bus, take_callback_stop};
use crate::ffi::{self, Rl78Reg};

static SEAT: Mutex<bool> = Mutex::new(false);
static TLIB_INIT: Once = Once::new();

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

/// RL78 CPU backed by tlib. At most one live instance may exist (tlib singleton).
pub struct Rl78Cpu {
    /// Held for the CPU lifetime. A second live `Rl78Cpu` panics in `TlibSeat::acquire`.
    _seat: TlibSeat,
    /// Callback-forced stops; see [`PendingStop`].
    pending: PendingStop,
    /// Kernel breakpoint table (id + addr). tlib only stores addresses, not
    /// [`BreakpointId`], so `EXCP_DEBUG` is mapped back to an id via PC lookup.
    breakpoints: Vec<Breakpoint>,
    /// True after [`Cpu::bind_memory`] has mapped the attached [`MemoryBus`].
    memory_bound: bool,
    /// Guest ranges passed to `tlib_map_range` while bound (unmapped on Drop).
    mapped_ranges: Vec<(u64, u64)>,
    /// Pages marked IO via `tlib_set_page_io_accessed` (cleared on Drop).
    io_pages: Vec<u64>,
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
        clear_io_bus();
        let _ = take_callback_stop();
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
            memory_bound: false,
            mapped_ranges: Vec::new(),
            io_pages: Vec::new(),
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
    /// # Safety
    /// `host` must remain valid for as long as the range stays mapped (the
    /// `Rom`/`Ram` allocation on the bus).
    pub unsafe fn map_memory(&mut self, guest_base: u64, length: u64, host: *mut u8) {
        unsafe {
            callbacks::map_host_region(guest_base, length, host);
            ffi::tlib_map_range(guest_base, length);
        }
        self.mapped_ranges.push((guest_base, length));
    }

    /// Mark every tlib page covering `[guest_base, guest_base + length)` as IO.
    pub fn set_io_pages(&mut self, guest_base: u64, length: u64) {
        if length == 0 {
            return;
        }
        let end = guest_base.saturating_add(length);
        let mut page = guest_base & !(TLIB_PAGE_SIZE - 1);
        while page < end {
            unsafe { ffi::tlib_set_page_io_accessed(page) };
            self.io_pages.push(page);
            page = page.saturating_add(TLIB_PAGE_SIZE);
            if page == 0 {
                break;
            }
        }
    }

    /// Tear down host/tlib mappings installed by [`Cpu::bind_memory`].
    ///
    /// Called from [`Drop`] so sequential unit-test Machines do not leave
    /// dangling host pointers or stale `tlib_map_range` windows after the bus
    /// is freed (CPU is dropped before the boxed bus).
    fn unbind_memory(&mut self) {
        clear_io_bus();
        let _ = take_callback_stop();
        for page in self.io_pages.drain(..) {
            unsafe { ffi::tlib_clear_page_io_accessed(page) };
        }
        for (base, size) in self.mapped_ranges.drain(..) {
            if size == 0 {
                continue;
            }
            let end = base.saturating_add(size).saturating_sub(1);
            unsafe { ffi::tlib_unmap_range(base, end) };
        }
        callbacks::clear_host_regions();
        self.memory_bound = false;
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
        if self.memory_bound {
            self.unbind_memory();
        } else {
            clear_io_bus();
            let _ = take_callback_stop();
        }
    }
}

impl Cpu for Rl78Cpu {
    fn bind_memory(&mut self, bus: &mut MemoryBus) {
        assert!(
            !self.memory_bound,
            "Rl78Cpu::bind_memory: memory already bound"
        );
        callbacks::clear_host_regions();
        let _ = take_callback_stop();

        let mut host_regions = Vec::new();
        let mut io_regions = Vec::new();
        bus.for_each_region(|base, size, host| {
            if let Some(ptr) = host {
                host_regions.push((base, size, ptr));
            } else {
                io_regions.push((base, size));
            }
        });

        for (base, size, ptr) in host_regions {
            unsafe {
                callbacks::map_host_region(base, size, ptr);
                ffi::tlib_map_range(base, size);
            }
            self.mapped_ranges.push((base, size));
        }

        for (base, size) in io_regions {
            // Leave MMIO physically unassigned so ABS16 stores (`rl78_write_byte` →
            // `stb_phys`) take the IO_MEM_UNASSIGNED path into host write
            // callbacks. Mapping as RAM would require a host pointer and would
            // bypass MagicProbe.
            self.set_io_pages(base, size);
        }

        // Safety: `Machine` boxes the bus before bind and keeps it pinned.
        unsafe { set_io_bus(bus as *mut MemoryBus) };
        self.memory_bound = true;
    }

    fn run_quantum(&mut self, max_instructions: u32) -> Quantum {
        let exit = unsafe { ffi::tlib_execute(max_instructions) };
        let instructions = unsafe { ffi::tlib_get_executed_instructions() };
        #[cfg(test)]
        if instructions > 0 {
            self.unit_test_guest_executed = true;
        }
        if let Some(reason) = take_callback_stop() {
            self.pending.request(reason);
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
    use serial_test::serial;
    use sim_kernel::{StopReason, TlibExit};

    #[serial]
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

    #[serial]
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

    #[serial]
    #[test]
    fn tlib_init_reports_rl78_arch() {
        let _cpu = Rl78Cpu::new();
        let arch = unsafe { std::ffi::CStr::from_ptr(ffi::tlib_get_arch()) };
        assert_eq!(arch.to_string_lossy(), "rl78");
    }

    #[serial]
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

    #[serial]
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

    #[serial]
    #[test]
    fn second_live_cpu_panics() {
        let _cpu = Rl78Cpu::new();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Rl78Cpu::new();
        }));
        assert!(panicked.is_err());
    }

    #[serial]
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
    use crate::{MAGIC_PROBE_BASE, MinimalMachineConfig, ProbeSink, minimal_machine_with_probe};
    use serial_test::serial;
    use sim_kernel::{Cpu, MemoryMapBuilder, Ram, Rom, StopReason, UnmappedPolicy};
    use std::sync::{Arc, Mutex};

    #[serial]
    #[test]
    fn bind_memory_maps_rom_host_region() {
        let mut bus = MemoryMapBuilder::new()
            .map(0, Box::new(Rom::from_bytes(vec![0u8; 4096])))
            .unwrap()
            .map(0xF0100, Box::new(Ram::new(1024)))
            .unwrap()
            .build();
        let mut cpu = Rl78Cpu::new();
        cpu.bind_memory(&mut bus);
        assert!(cpu.memory_bound);
        let mapped = unsafe { ffi::tlib_is_range_mapped(0, 1) };
        assert_ne!(mapped, 0);
        let p = crate::callbacks::rl78_host_guest_offset_to_host_ptr(0);
        assert!(!p.is_null());
        // Do not call `run_quantum` here: after a guest execute, later tests in
        // this process that touch tlib can SIGSEGV (stale TCG). Guest execute
        // coverage lives in `guest_magic_probe_smoke`.
    }

    #[derive(Clone, Default)]
    struct BufferSink {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl ProbeSink for BufferSink {
        fn emit(&mut self, bytes: &[u8]) {
            self.buf
                .lock()
                .expect("probe buffer")
                .extend_from_slice(bytes);
        }
    }

    #[serial]
    #[test]
    fn bind_memory_routes_magic_probe_via_io() {
        let sink = BufferSink::default();
        let captured = Arc::clone(&sink.buf);
        let mut machine = minimal_machine_with_probe(MinimalMachineConfig::default(), sink);
        machine
            .bus_mut()
            .write(MAGIC_PROBE_BASE, b"via-bus")
            .unwrap();
        assert_eq!(&captured.lock().unwrap()[..], b"via-bus");
        captured.lock().unwrap().clear();

        // IO path installed by bind_memory → set_io_bus (no tlib_set_return_request).
        assert!(crate::callbacks::rl78_host_write_byte_for_test(MAGIC_PROBE_BASE, b'h').is_ok());
        assert!(
            crate::callbacks::rl78_host_write_byte_for_test(MAGIC_PROBE_BASE + 1, b'i').is_ok()
        );
        assert_eq!(&captured.lock().unwrap()[..], b"hi");
    }

    #[serial]
    #[test]
    fn unmapped_io_latches_stop() {
        let mut machine = minimal_machine_with_probe(
            MinimalMachineConfig {
                unmapped: UnmappedPolicy::Trap,
                ..MinimalMachineConfig::default()
            },
            BufferSink::default(),
        );
        assert!(crate::callbacks::rl78_host_write_byte_for_test(0x80000, 0xAA).is_err());
        let reason = take_callback_stop();
        assert_eq!(
            reason,
            Some(StopReason::Unmapped {
                addr: 0x80000,
                write: true
            })
        );
        assert!(machine.bus_mut().take_trap().is_some());
    }

    #[serial]
    #[test]
    fn drop_clears_bound_host_mappings() {
        let mut bus = MemoryMapBuilder::new()
            .map(0, Box::new(Rom::from_bytes(vec![0u8; 4096])))
            .unwrap()
            .map(0xF0100, Box::new(Ram::new(1024)))
            .unwrap()
            .build();
        let mut cpu = Rl78Cpu::new();
        cpu.bind_memory(&mut bus);
        assert!(!crate::callbacks::rl78_host_guest_offset_to_host_ptr(0).is_null());
        drop(cpu);
        assert!(
            crate::callbacks::rl78_host_guest_offset_to_host_ptr(0).is_null(),
            "host regions must be cleared on Drop before the bus is freed"
        );
        let mapped = unsafe { ffi::tlib_is_range_mapped(0, 1) };
        assert_eq!(mapped, 0, "tlib map for ROM must be unmapped on Drop");
    }
}
