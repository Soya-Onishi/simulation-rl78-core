//! Host-side tlib callbacks (phase C scaffolding).
//!
//! C shims in `host_callbacks.c` (whole-archived) override the weak stubs inside
//! `libtlib.a` and forward here. Direct-mapped guest regions use
//! [`map_host_region`]. IO-page accesses go through [`IoHandler`] (phase D will
//! route these into [`sim_kernel::MemoryBus`]).

use std::os::raw::c_char;
use std::ptr;
use std::sync::{Mutex, OnceLock};

use crate::ffi;

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

/// IO-page load/store hook. Return `Err(())` to request `tlib_set_return_request`.
pub trait IoHandler: Send {
    fn read(&mut self, addr: u64, width: u8) -> Result<u64, ()>;
    fn write(&mut self, addr: u64, value: u64, width: u8) -> Result<(), ()>;
}

struct CallbackState {
    regions: Vec<HostRegion>,
    io: Option<Box<dyn IoHandler>>,
}

impl CallbackState {
    fn new() -> Self {
        Self {
            regions: Vec::new(),
            io: None,
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

/// Drop all host region registrations (does not call `tlib_unmap_range`).
///
/// Prefer scoped tests that do not touch process-wide callback state. After
/// clearing, the next [`crate::cpu::Rl78Cpu::new`] re-registers the default
/// NOP window host pointer.
pub fn clear_host_regions() {
    state().lock().expect("tlib callback state").regions.clear();
}

/// Install / replace the IO-page handler. Pass `None` to clear.
pub fn set_io_handler(handler: Option<Box<dyn IoHandler>>) {
    state().lock().expect("tlib callback state").io = handler;
}

fn io_read(addr: u64, width: u8) -> Result<u64, ()> {
    let mut guard = state().lock().expect("tlib callback state");
    match guard.io.as_mut() {
        Some(io) => io.read(addr, width),
        None => {
            // Mapped RAM should be served by guest_offset; IO without a handler
            // returns 0 (matches tlib reset-vector behavior before mapping).
            let _ = (addr, width);
            Ok(0)
        }
    }
}

fn io_write(addr: u64, value: u64, width: u8) -> Result<(), ()> {
    let mut guard = state().lock().expect("tlib callback state");
    match guard.io.as_mut() {
        Some(io) => io.write(addr, value, width),
        None => {
            eprintln!("rl78-core: unhandled IO write addr={addr:#x} value={value:#x} width={width}");
            Err(())
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rl78_host_guest_offset_to_host_ptr(offset: u64) -> *mut std::os::raw::c_void {
    match state().lock().expect("tlib callback state").find_host(offset) {
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
        // Do not call clear_host_regions() here: it wipes process-wide state and
        // races with parallel Rl78Cpu tests that rely on the default NOP window.
    }
}
