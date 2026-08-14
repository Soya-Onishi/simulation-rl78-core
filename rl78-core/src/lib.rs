//! RL78 core assembly: CPU wrapper, minimal map, Magic probe, ELF hook.
//!
//! tlib (`rl78` branch) is built from the `tlib/` submodule via `build.rs` and
//! linked statically. [`Rl78Cpu`] owns the tlib session; [`Cpu::bind_memory`]
//! wires ROM/RAM/MMIO into the bus.

mod callbacks;
mod cpu;
mod elf;
mod ffi;
mod magic;
mod map;
mod peripherals;

pub use callbacks::{
    HostRegion, TLIB_PAGE_SIZE, clear_host_regions, clear_io_bus, map_host_region,
    remove_host_region, set_io_bus, take_callback_stop,
};
pub use cpu::Rl78Cpu;
pub use elf::{EM_RL78, ElfLoad, LoadError, load_elf, load_elf_into_machine};
pub use ffi::{Rl78Reg, excp};
pub use magic::{MagicProbe, ProbeSink, StdoutSink};
pub use map::{
    ESFR_BASE, ESFR_SIZE, MAGIC_PROBE_BASE, MAGIC_PROBE_SIZE, MemoryLayout, Rl78Device, SFR_BASE,
    SFR_SIZE,
};
pub use peripherals::{ClockGenerator, ClockTree, G23Peripherals, SauUnit, TauUnit};

use std::sync::{Arc, Mutex};

use sim_kernel::{Machine, MapError, MemoryBus, MemoryMapBuilder, Ram, Rom, UnmappedPolicy};

use crate::peripherals::g23_sfr_windows;

/// Knobs for [`minimal_machine`]. All configuration is code, not a file.
#[derive(Clone, Debug, Default)]
pub struct MinimalMachineConfig {
    /// Selects ROM/RAM windows for the target core / part.
    pub device: Rl78Device,
    /// Optional override; when `None`, [`Rl78Device::memory_layout`] is used.
    pub layout: Option<MemoryLayout>,
    pub unmapped: UnmappedPolicy,
}

impl MinimalMachineConfig {
    #[must_use]
    pub fn layout(&self) -> MemoryLayout {
        self.layout.unwrap_or_else(|| self.device.memory_layout())
    }
}

/// Build the milestone-1 machine: ROM + RAM + Magic probe on one bus.
///
/// The memory map is assembled as an immutable [`MemoryMapBuilder`] chain and
/// handed to [`Machine::new`] in one step. Board crates can build their own
/// map the same way and construct a `Machine` directly.
pub fn minimal_machine(cfg: MinimalMachineConfig) -> Machine<Rl78Cpu> {
    minimal_machine_with_probe(cfg, StdoutSink)
}

/// Same as [`minimal_machine`] but the probe writes into `sink` (for tests).
pub fn minimal_machine_with_probe<S>(cfg: MinimalMachineConfig, sink: S) -> Machine<Rl78Cpu>
where
    S: ProbeSink + 'static,
{
    let bus = build_minimal_bus(&cfg, sink).expect("minimal memory map");
    Machine::new(Rl78Cpu::new(), bus)
}

/// Assemble the M1 memory map without creating a [`Machine`].
pub fn build_minimal_bus<S>(cfg: &MinimalMachineConfig, sink: S) -> Result<MemoryBus, MapError>
where
    S: ProbeSink + 'static,
{
    let layout = cfg.layout();
    MemoryMapBuilder::new()
        .policy(cfg.unmapped)
        .map(layout.rom_base, Box::new(Rom::new(layout.rom_size)))?
        .map(layout.ram_base, Box::new(Ram::new(layout.ram_size)))?
        .map(MAGIC_PROBE_BASE, Box::new(MagicProbe::new(sink)))
        .map(|b| b.build())
}

/// Knobs for [`g23_machine`]. Option byte `0x000C2` feeds the clock generator.
#[derive(Clone, Debug, Default)]
pub struct G23MachineConfig {
    pub unmapped: UnmappedPolicy,
    /// Flash option byte at `0x000C2` (`FRQSEL`). `None` matches QEMU when ROM is empty.
    pub option_byte: Option<u8>,
}

/// RL78/G23-class machine: ROM/RAM + clock/SAU/TAU SFR windows (no Magic probe).
pub fn g23_machine(cfg: G23MachineConfig) -> Machine<Rl78Cpu> {
    let bus = build_g23_bus(&cfg).expect("g23 memory map");
    Machine::new(Rl78Cpu::new(), bus)
}

/// Assemble the G23 map without creating a [`Machine`].
pub fn build_g23_bus(cfg: &G23MachineConfig) -> Result<MemoryBus, MapError> {
    let layout = Rl78Device::R7F100Gxl.memory_layout();
    let peri = Arc::new(Mutex::new(G23Peripherals::from_option_byte(
        cfg.option_byte,
    )));
    let (sfr, esfr) = g23_sfr_windows(peri);
    Ok(MemoryMapBuilder::new()
        .policy(cfg.unmapped)
        .map(layout.rom_base, Box::new(Rom::new(layout.rom_size)))?
        .map(layout.ram_base, Box::new(Ram::new(layout.ram_size)))?
        .map(ESFR_BASE, Box::new(esfr))?
        .map(SFR_BASE, Box::new(sfr))?
        .build())
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use std::sync::{Arc, Mutex};

    use super::*;
    use sim_kernel::BusError;

    #[derive(Clone, Default)]
    struct BufferSink {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl BufferSink {
        fn buffer(&self) -> Arc<Mutex<Vec<u8>>> {
            Arc::clone(&self.buf)
        }
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
    fn magic_probe_is_a_normal_mapped_device() {
        let sink = BufferSink::default();
        let captured = sink.buffer();
        let mut machine = minimal_machine_with_probe(MinimalMachineConfig::default(), sink);
        machine.bus_mut().write(MAGIC_PROBE_BASE, b"hello").unwrap();
        assert_eq!(captured.lock().expect("probe lock").as_slice(), b"hello");
    }

    #[serial]
    #[test]
    fn unmapped_access_is_logged() {
        let mut machine = minimal_machine(MinimalMachineConfig::default());
        let err = machine.bus_mut().read(0x80000, &mut [0u8; 1]).unwrap_err();
        assert!(matches!(err, BusError::Unmapped { addr: 0x80000, .. }));
        assert_eq!(machine.bus_mut().take_unmapped_log().len(), 1);
    }

    #[serial]
    #[test]
    fn device_layout_is_selectable() {
        let layout = Rl78Device::Generic64k.memory_layout();
        assert_eq!(layout.rom_size, 64 * 1024);
        assert_eq!(layout.ram_size, 32 * 1024);
        assert_eq!(layout.ram_base, 0xF0100);
        let custom = MemoryLayout {
            rom_base: 0,
            rom_size: 8 * 1024,
            ram_base: 0xF8000,
            ram_size: 4 * 1024,
        };
        let cfg = MinimalMachineConfig {
            layout: Some(custom),
            ..MinimalMachineConfig::default()
        };
        assert_eq!(cfg.layout().ram_size, 4 * 1024);
        let _ = minimal_machine(cfg);
    }

    #[test]
    fn g23_clock_and_timer_sfr_are_mapped() {
        let mut bus = build_g23_bus(&G23MachineConfig::default()).unwrap();
        let mut ckc = [0u8; 1];
        bus.read(0xFFFA4, &mut ckc).unwrap();
        assert_eq!(ckc[0], 0);

        bus.write(0xF00A8, &[1]).unwrap();
        let mut hocodiv = [0u8; 1];
        bus.read(0xF00A8, &mut hocodiv).unwrap();
        assert_eq!(hocodiv[0], 1);

        bus.write(0xF01B2, &[0x01, 0x00]).unwrap();
        let mut te = [0u8; 2];
        bus.read(0xF01B0, &mut te).unwrap();
        assert_eq!(te, [1, 0]);

        bus.write(0xFFF10, &[0x5A, 0x00]).unwrap();
        let mut sdr = [0u8; 2];
        bus.read(0xFFF10, &mut sdr).unwrap();
        assert_eq!(sdr[0], 0x5A);
    }

    #[test]
    fn r7f100gxl_ram_does_not_cover_sau() {
        let layout = Rl78Device::R7F100Gxl.memory_layout();
        assert_eq!(layout.ram_base, 0xF3F00);
        assert!(layout.ram_base > 0xF0100);
    }
}
