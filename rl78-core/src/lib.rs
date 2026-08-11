//! RL78 core assembly: CPU wrapper, minimal map, Magic probe, ELF hook.
//!
//! tlib is not linked in this crate yet (issue #1 phase C). The [`Rl78Cpu`]
//! type and [`minimal_machine`] builder are the stable surface later PRs fill in.

mod cpu;
mod elf;
mod magic;
mod map;

pub use cpu::{REG_PC, REG_PSW, REG_SP, Rl78Cpu};
pub use elf::{ElfLoad, LoadError, load_elf};
pub use magic::{BufferSink, MagicProbe, ProbeSink, StdoutSink};
pub use map::{MAGIC_PROBE_BASE, MAGIC_PROBE_SIZE, RAM_BASE, RAM_SIZE, ROM_BASE, ROM_SIZE};

use sim_kernel::{Machine, Ram, Rom};

/// Knobs for [`minimal_machine`]. All configuration is code, not a file.
#[derive(Clone, Debug)]
pub struct MinimalMachineConfig {
    pub rom_size: usize,
    pub ram_size: usize,
}

impl Default for MinimalMachineConfig {
    fn default() -> Self {
        Self {
            rom_size: ROM_SIZE,
            ram_size: RAM_SIZE,
        }
    }
}

/// Build the milestone-1 machine: ROM + RAM + Magic probe on one bus.
///
/// Higher-level board crates are expected to take this `Machine` by value and
/// map additional devices onto [`Machine::bus_mut`].
pub fn minimal_machine(cfg: MinimalMachineConfig) -> Machine<Rl78Cpu> {
    let mut machine = Machine::new(Rl78Cpu::new());
    machine
        .bus_mut()
        .map(ROM_BASE, Box::new(Rom::new(cfg.rom_size)))
        .expect("ROM mapping");
    machine
        .bus_mut()
        .map(RAM_BASE, Box::new(Ram::new(cfg.ram_size)))
        .expect("RAM mapping");
    machine
        .bus_mut()
        .map(MAGIC_PROBE_BASE, Box::new(MagicProbe::new(StdoutSink)))
        .expect("Magic probe mapping");
    machine
}

/// Same as [`minimal_machine`] but the probe writes into `sink` (for tests).
pub fn minimal_machine_with_probe<S>(cfg: MinimalMachineConfig, sink: S) -> Machine<Rl78Cpu>
where
    S: ProbeSink + 'static,
{
    let mut machine = Machine::new(Rl78Cpu::new());
    machine
        .bus_mut()
        .map(ROM_BASE, Box::new(Rom::new(cfg.rom_size)))
        .expect("ROM mapping");
    machine
        .bus_mut()
        .map(RAM_BASE, Box::new(Ram::new(cfg.ram_size)))
        .expect("RAM mapping");
    machine
        .bus_mut()
        .map(MAGIC_PROBE_BASE, Box::new(MagicProbe::new(sink)))
        .expect("Magic probe mapping");
    machine
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{BusError, UnmappedPolicy};

    #[test]
    fn magic_probe_is_a_normal_mapped_device() {
        let sink = BufferSink::default();
        let captured = sink.buffer();
        let mut machine = minimal_machine_with_probe(MinimalMachineConfig::default(), sink);
        machine.bus_mut().write(MAGIC_PROBE_BASE, b"hello").unwrap();
        assert_eq!(captured.lock().expect("probe lock").as_slice(), b"hello");
    }

    #[test]
    fn unmapped_access_is_logged() {
        let mut machine = minimal_machine(MinimalMachineConfig::default());
        machine.bus_mut().set_policy(UnmappedPolicy::Log);
        let err = machine.bus_mut().read(0x80000, &mut [0u8; 1]).unwrap_err();
        assert!(matches!(err, BusError::Unmapped { addr: 0x80000, .. }));
        assert_eq!(machine.bus_mut().take_unmapped_log().len(), 1);
    }

    #[test]
    fn load_elf_is_stubbed() {
        let mut machine = minimal_machine(MinimalMachineConfig::default());
        assert!(matches!(
            load_elf(b"\0ELF", machine.bus_mut()),
            Err(LoadError::NotImplemented)
        ));
    }
}
