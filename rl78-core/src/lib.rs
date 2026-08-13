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

pub use callbacks::{
    BusIoHandler, HostRegion, IoHandler, TLIB_PAGE_SIZE, clear_host_regions, map_host_region,
    remove_host_region, set_io_handler, take_callback_stop,
};
pub use cpu::Rl78Cpu;
pub use elf::{
    EM_RL78, ElfLoad, LoadError, load_elf, load_elf_into_machine, magic_probe_guest_code,
    write_minimal_elf32,
};
pub use ffi::{Rl78Reg, excp};
pub use magic::{MagicProbe, ProbeSink, StdoutSink};
pub use map::{MAGIC_PROBE_BASE, MAGIC_PROBE_SIZE, MemoryLayout, Rl78Device};

use sim_kernel::{Machine, MapError, MemoryBus, MemoryMapBuilder, Ram, Rom, UnmappedPolicy};

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

    #[serial]
    #[test]
    fn load_elf_roundtrips_via_minimal_machine() {
        let code = magic_probe_guest_code(b"x");
        let image = write_minimal_elf32(0x100, 0x100, &code);
        let mut machine = minimal_machine(MinimalMachineConfig::default());
        let loaded = load_elf_into_machine(&image, &mut machine).unwrap();
        assert_eq!(loaded.entry, 0x100);
        let mut buf = [0u8; 1];
        machine.bus_mut().read(0x100, &mut buf).unwrap();
        assert_eq!(buf[0], 0xcf);
    }
}
