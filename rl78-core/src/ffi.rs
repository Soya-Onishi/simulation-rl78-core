//! Raw FFI to the statically linked tlib (RL78).
//!
//! Declarations mirror `tlib/include/exports.h` and the RL78 register helpers.
//! Call sites live in [`crate::cpu`]; memory callbacks are in [`crate::callbacks`].

#![allow(dead_code)] // full export surface kept for phases D–G
#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int};

/// Subset of `include/cpu-defs.h` exit codes (also mirrored by `sim_kernel::TlibExit`).
pub mod excp {
    pub const INTERRUPT: i32 = 0x10000;
    pub const WFI: i32 = 0x10001;
    pub const DEBUG: i32 = 0x10002;
    pub const WATCHPOINT: i32 = 0x10004;
    pub const RETURN_REQUEST: i32 = 0x10005;
}

/// RL78 `Registers32` indices from `arch/rl78/cpu_registers.h`.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rl78Reg {
    X = 0,
    A = 1,
    C = 2,
    B = 3,
    E = 4,
    D = 5,
    L = 6,
    H = 7,
    Pc = 8,
    Sp = 9,
    Es = 10,
    Cs = 11,
    PswCy = 12,
    PswIsp = 13,
    PswRbs = 14,
    PswAc = 15,
    PswZ = 16,
    PswIe = 17,
}

unsafe extern "C" {
    pub fn tlib_init(cpu_name: *mut c_char) -> i32;
    pub fn tlib_dispose();
    pub fn tlib_reset();
    pub fn tlib_execute(max_insns: u32) -> i32;
    pub fn tlib_get_executed_instructions() -> u64;
    pub fn tlib_set_return_request();
    pub fn tlib_map_range(start_addr: u64, length: u64);
    pub fn tlib_unmap_range(start: u64, end: u64);
    pub fn tlib_is_range_mapped(start: u64, end: u64) -> u32;
    pub fn tlib_invalidate_translation_cache();
    pub fn tlib_set_page_io_accessed(address: u64);
    pub fn tlib_clear_page_io_accessed(address: u64);
    pub fn tlib_add_breakpoint(address: u64);
    pub fn tlib_remove_breakpoint(address: u64);
    pub fn tlib_breakpoint_count() -> usize;
    pub fn tlib_list_breakpoints(addrs: *mut u64, capacity: usize) -> usize;
    pub fn tlib_get_arch() -> *mut c_char;
    pub fn tlib_get_register_value_32(reg_number: c_int) -> u32;
    pub fn tlib_set_register_value_32(reg_number: c_int, value: u32);
    pub fn tlib_get_register_value(reg_number: c_int) -> u64;
    pub fn tlib_set_register_value(reg_number: c_int, value: u64);
    pub fn tlib_set_rl78_irq(index: i32, priority: i32, enable: i32);
}
