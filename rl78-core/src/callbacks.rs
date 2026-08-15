//! Host-side tlib callbacks.
//!
//! C shims in `host_callbacks.c` (whole-archived) override the weak stubs inside
//! `libtlib.a` and forward here. Direct-mapped guest regions use
//! [`map_host_region`]. IO-page accesses go straight to a bound
//! [`sim_kernel::MemoryBus`] (width packing lives in this module).

use std::os::raw::c_char;
use std::ptr;
use std::sync::{Mutex, OnceLock};

use sim_kernel::{Addr, BusError, MemoryBus, StopReason};

use crate::ffi;

/// RL78 tlib page size (`TARGET_PAGE_BITS = 8`).
pub const TLIB_PAGE_SIZE: u64 = 256;

/// One contiguous guest physical window backed by a host buffer.
#[derive(Clone, Copy, Debug)]
pub struct HostRegion {
    pub guest_base: u64,
    pub size: u64,
    pub host: *mut u8,
}

// Safety: regions are registered/unregistered only while the sim thread owns tlib,
// and host pointers outlive the mapping (CPU / backing store lifetime).
unsafe impl Send for HostRegion {}
unsafe impl Sync for HostRegion {}

fn latch_bus_error(_addr: Addr, write: bool, err: &BusError) {
    match err {
        BusError::Unmapped { addr, .. } => {
            request_callback_stop(StopReason::Unmapped { addr: *addr, write });
        }
        BusError::ReadOnly { addr } => {
            // Treat ROM/MMIO reject as an unmapped-style guest fault for M1.
            request_callback_stop(StopReason::Unmapped {
                addr: *addr,
                write: true,
            });
        }
        BusError::OutOfRange { addr, .. } => {
            request_callback_stop(StopReason::Unmapped { addr: *addr, write });
        }
        BusError::NotLoadable { addr } => {
            request_callback_stop(StopReason::Unmapped {
                addr: *addr,
                write: true,
            });
        }
    }
}

struct CallbackState {
    regions: Vec<HostRegion>,
    /// Bound by [`set_io_bus`] from [`crate::Cpu::bind_memory`].
    /// `Machine` boxes the bus before bind and does not move it afterward.
    io_bus: Option<*mut MemoryBus>,
}

// Safety: `io_bus` is only used on the sim thread while the `Machine` lives.
unsafe impl Send for CallbackState {}

impl CallbackState {
    fn new() -> Self {
        Self {
            regions: Vec::new(),
            io_bus: None,
        }
    }

    fn find_host(&self, addr: u64) -> Option<*mut u8> {
        for r in &self.regions {
            if addr >= r.guest_base && addr < r.guest_base.saturating_add(r.size) {
                let off = (addr - r.guest_base) as usize;
                return Some(unsafe { r.host.add(off) });
            }
        }
        None
    }
}

fn state() -> &'static Mutex<CallbackState> {
    static STATE: OnceLock<Mutex<CallbackState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(CallbackState::new()))
}

/// Separate from [`state`] so IO helpers can latch a stop without re-locking
/// the region/bus mutex.
fn stop_latch() -> &'static Mutex<Option<StopReason>> {
    static STOP: OnceLock<Mutex<Option<StopReason>>> = OnceLock::new();
    STOP.get_or_init(|| Mutex::new(None))
}

/// Record a stop from an IO callback (merged into [`sim_kernel::PendingStop`] after execute).
pub fn request_callback_stop(reason: StopReason) {
    let mut guard = stop_latch().lock().expect("callback stop latch");
    if guard.is_none() {
        *guard = Some(reason);
    }
}

/// Take a stop latched by an IO callback, if any.
pub fn take_callback_stop() -> Option<StopReason> {
    stop_latch().lock().expect("callback stop latch").take()
}

/// Register a host-backed guest window for TCG direct access.
///
/// # Safety
/// `host` must remain valid, uniquely used, and not move for the lifetime of
/// the mapping. tlib's TCG path calls [`rl78_host_guest_offset_to_host_ptr`]
/// and then **dereferences the returned pointer** during `tlib_execute` (it
/// does not go through [`sim_kernel::MemoryBus`]). A dangling or aliased
/// pointer is therefore undefined behavior in both Rust and tlib.
pub unsafe fn map_host_region(guest_base: u64, size: u64, host: *mut u8) {
    let mut guard = state().lock().expect("tlib callback state");
    guard.regions.retain(|r| {
        let end = r.guest_base.saturating_add(r.size);
        let new_end = guest_base.saturating_add(size);
        end <= guest_base || new_end <= r.guest_base
    });
    guard.regions.push(HostRegion {
        guest_base,
        size,
        host,
    });
}

/// Remove a previously registered host-backed guest window.
pub fn remove_host_region(guest_base: u64, size: u64) {
    let mut guard = state().lock().expect("tlib callback state");
    guard
        .regions
        .retain(|r| r.guest_base != guest_base || r.size != size);
}

/// Drop all host region registrations (does not call `tlib_unmap_range`).
pub fn clear_host_regions() {
    state().lock().expect("tlib callback state").regions.clear();
}

/// Bind the [`MemoryBus`] used for IO-page callbacks.
///
/// # Safety
/// `bus` must remain valid and uniquely used until [`clear_io_bus`] (`Machine`
/// boxes the bus before [`crate::Cpu::bind_memory`]).
pub unsafe fn set_io_bus(bus: *mut MemoryBus) {
    state().lock().expect("tlib callback state").io_bus = Some(bus);
}

/// Clear the IO-page bus pointer.
pub fn clear_io_bus() {
    state().lock().expect("tlib callback state").io_bus = None;
}

fn width_len(width: u8) -> Result<usize, ()> {
    match width {
        1 | 2 | 4 | 8 => Ok(usize::from(width)),
        _ => Err(()),
    }
}

fn width_mask(width: u8) -> u64 {
    match width {
        1 => 0xff,
        2 => 0xffff,
        4 => 0xffff_ffff,
        _ => u64::MAX,
    }
}

fn io_bus_ptr() -> Option<*mut MemoryBus> {
    state().lock().expect("tlib callback state").io_bus
}

#[allow(clippy::result_unit_err)]
fn io_read(addr: u64, width: u8) -> Result<u64, ()> {
    let Some(bus) = io_bus_ptr() else {
        // Mapped RAM should be served by guest_offset; IO before bind returns 0.
        return Ok(0);
    };
    let bus = unsafe { &mut *bus };
    let mut buf = [0u8; 8];
    let len = width_len(width)?;
    match bus.read(addr, &mut buf[..len]) {
        Ok(()) => Ok(u64::from_le_bytes(buf) & width_mask(width)),
        Err(err) => {
            latch_bus_error(addr, false, &err);
            Err(())
        }
    }
}

#[allow(clippy::result_unit_err)]
fn io_write(addr: u64, value: u64, width: u8) -> Result<(), ()> {
    let Some(bus) = io_bus_ptr() else {
        eprintln!("rl78-core: unhandled IO write addr={addr:#x} value={value:#x} width={width}");
        return Err(());
    };
    let bus = unsafe { &mut *bus };
    let len = width_len(width)?;
    let bytes = value.to_le_bytes();
    match bus.write(addr, &bytes[..len]) {
        Ok(()) => Ok(()),
        Err(err) => {
            latch_bus_error(addr, true, &err);
            Err(())
        }
    }
}

/// Test helper: IO-page write without calling `tlib_set_return_request`.
#[cfg(test)]
#[allow(clippy::result_unit_err)]
pub(crate) fn rl78_host_write_byte_for_test(address: u64, value: u8) -> Result<(), ()> {
    io_write(address, u64::from(value), 1)
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_guest_offset_to_host_ptr(offset: u64) -> *mut std::os::raw::c_void {
    match state()
        .lock()
        .expect("tlib callback state")
        .find_host(offset)
    {
        Some(p) => p.cast(),
        None => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_read_byte(address: u64, _cpustate: u64) -> u64 {
    match io_read(address, 1) {
        Ok(v) => v & 0xff,
        Err(()) => {
            unsafe { ffi::tlib_set_return_request() };
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_read_word(address: u64, _cpustate: u64) -> u64 {
    match io_read(address, 2) {
        Ok(v) => v & 0xffff,
        Err(()) => {
            unsafe { ffi::tlib_set_return_request() };
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_read_double_word(address: u64, _cpustate: u64) -> u64 {
    match io_read(address, 4) {
        Ok(v) => v & 0xffff_ffff,
        Err(()) => {
            unsafe { ffi::tlib_set_return_request() };
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_read_quad_word(address: u64, _cpustate: u64) -> u64 {
    match io_read(address, 8) {
        Ok(v) => v,
        Err(()) => {
            unsafe { ffi::tlib_set_return_request() };
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_write_byte(address: u64, value: u64, _cpustate: u64) {
    if io_write(address, value & 0xff, 1).is_err() {
        unsafe { ffi::tlib_set_return_request() };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_write_word(address: u64, value: u64, _cpustate: u64) {
    if io_write(address, value & 0xffff, 2).is_err() {
        unsafe { ffi::tlib_set_return_request() };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_write_double_word(address: u64, value: u64, _cpustate: u64) {
    if io_write(address, value & 0xffff_ffff, 4).is_err() {
        unsafe { ffi::tlib_set_return_request() };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_write_quad_word(address: u64, value: u64, _cpustate: u64) {
    if io_write(address, value, 8).is_err() {
        unsafe { ffi::tlib_set_return_request() };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_abort(message: *mut c_char) {
    let msg = if message.is_null() {
        "<null>".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    };
    eprintln!("tlib abort: {msg}");
    panic!("tlib abort: {msg}");
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_on_rl78_irq_ack(index: u32) {
    crate::peripherals::irq::on_tlib_ack(index);
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_log(_level: i32, message: *mut c_char) {
    if message.is_null() {
        return;
    }
    let msg = unsafe { std::ffi::CStr::from_ptr(message) };
    eprintln!("tlib: {}", msg.to_string_lossy());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_region_lookup_offsets() {
        let mut buf = [0u8; 16];
        unsafe {
            map_host_region(0x1000, 16, buf.as_mut_ptr());
        }
        let p = rl78_host_guest_offset_to_host_ptr(0x1004);
        assert!(!p.is_null());
        assert_eq!(p as usize, buf.as_mut_ptr() as usize + 4);
        assert!(rl78_host_guest_offset_to_host_ptr(0x2000).is_null());
        remove_host_region(0x1000, 16);
        assert!(rl78_host_guest_offset_to_host_ptr(0x1004).is_null());
    }
}
