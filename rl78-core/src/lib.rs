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
pub use map::{MAGIC_PROBE_BASE, MAGIC_PROBE_SIZE, MemoryLayout, Rl78Device};
pub use peripherals::{
    ByteCapture, CaptureTx, ClockGenerator, ClockOutputs, ClockTree, Cycles, Hertz, IrqController,
    IrqId, IrqPulse, IrqRequest, R7F100Gxl, Rl78G23Core, SauUnit, TauUnit,
};

use sim_kernel::{
    EventCtl, HasMemoryMap, Machine, MapError, MemoryBus, MemoryMapBuilder, Ram, Rom,
    UnmappedPolicy,
};

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
    Machine::new(Rl78Cpu::new(), bus, EventCtl::new())
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

/// Knobs for [`g23_machine`]. Option byte `0x000C2` is read from ROM at reset.
#[derive(Clone, Debug, Default)]
pub struct G23MachineConfig {
    pub unmapped: UnmappedPolicy,
}

/// RL78/G23 + R7F100GxL RAM/ROM. Option byte is applied at core reset from ROM.
pub fn g23_machine(cfg: G23MachineConfig) -> Machine<Rl78Cpu> {
    let ctl = EventCtl::new();
    let mut part = R7F100Gxl::new(ctl.clone());
    let mut bus = part
        .memory_map()
        .expect("g23 memory map")
        .policy(cfg.unmapped)
        .build();
    part.reset(&mut bus);
    let irq = std::sync::Arc::clone(&part.core.irq);
    let machine = Machine::new(Rl78Cpu::new(), bus, ctl);
    peripherals::irq::bind_cpu_line(irq);
    machine
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use std::sync::{Arc, Mutex};

    use super::*;
    use sim_kernel::{BusError, HasMemoryMap};

    fn g23_part(ctl: EventCtl) -> (R7F100Gxl, MemoryBus) {
        let mut part = R7F100Gxl::new(ctl);
        let mut bus = part.memory_map().unwrap().build();
        part.reset(&mut bus);
        (part, bus)
    }

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
        let ctl = EventCtl::new();
        let (_, mut bus) = g23_part(ctl);
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
        let layout = R7F100Gxl::memory_layout();
        assert_eq!(layout.ram_base, 0xF3F00);
        assert!(layout.ram_base > 0xF0100);
    }

    fn pump(bus: &mut MemoryBus, ctl: &EventCtl, now: sim_kernel::Tick) {
        ctl.set_now(now);
        loop {
            let due = ctl.events().pop_due(now);
            let Some((_, mut event)) = due else {
                break;
            };
            let mut ctx = sim_kernel::EventCtx {
                now,
                bus,
                stop: None,
            };
            event.fire(&mut ctx);
        }
    }

    #[test]
    fn tau_interval_sets_overflow_after_tdr_counts() {
        let ctl = EventCtl::new();
        let (part, mut bus) = g23_part(ctl.clone());
        bus.write(0xFFF18, &[31, 0]).unwrap();
        bus.write(0xF01B2, &[0x01, 0x00]).unwrap();
        pump(&mut bus, &ctl, sim_kernel::Tick(0));
        pump(&mut bus, &ctl, sim_kernel::Tick(999));
        let mut tsr = [0u8; 2];
        bus.read(0xF01A0, &mut tsr).unwrap();
        assert_eq!(tsr[0] & 1, 0);
        pump(&mut bus, &ctl, sim_kernel::Tick(1000));
        bus.read(0xF01A0, &mut tsr).unwrap();
        assert_eq!(tsr[0] & 1, 1);
        assert!(part.core.tau.lock().unwrap().channel_enabled(0));
    }

    #[test]
    fn tau_restart_after_tt_ignores_stale_deadline() {
        let ctl = EventCtl::new();
        let (_, mut bus) = g23_part(ctl.clone());
        bus.write(0xFFF18, &[31, 0]).unwrap();
        bus.write(0xF01B2, &[0x01, 0x00]).unwrap();
        pump(&mut bus, &ctl, sim_kernel::Tick(0));
        bus.write(0xF01B4, &[0x01, 0x00]).unwrap();
        pump(&mut bus, &ctl, sim_kernel::Tick(0));
        pump(&mut bus, &ctl, sim_kernel::Tick(1000));
        let mut tsr = [0u8; 2];
        bus.read(0xF01A0, &mut tsr).unwrap();
        assert_eq!(tsr[0] & 1, 0);
        bus.write(0xF01B2, &[0x01, 0x00]).unwrap();
        pump(&mut bus, &ctl, sim_kernel::Tick(1000));
        pump(&mut bus, &ctl, sim_kernel::Tick(1999));
        bus.read(0xF01A0, &mut tsr).unwrap();
        assert_eq!(tsr[0] & 1, 0);
        pump(&mut bus, &ctl, sim_kernel::Tick(2000));
        bus.read(0xF01A0, &mut tsr).unwrap();
        assert_eq!(tsr[0] & 1, 1);
    }

    #[test]
    fn tau_arms_when_fclk_returns() {
        let ctl = EventCtl::new();
        let (_, mut bus) = g23_part(ctl.clone());
        bus.write(0xFFFA1, &[0xC1]).unwrap();
        bus.write(0xFFF18, &[31, 0]).unwrap();
        bus.write(0xF01B2, &[0x01, 0x00]).unwrap();
        pump(&mut bus, &ctl, sim_kernel::Tick(0));
        bus.write(0xFFFA1, &[0xC0]).unwrap();
        bus.write(0xF01B2, &[0x01, 0x00]).unwrap();
        pump(&mut bus, &ctl, sim_kernel::Tick(0));
        pump(&mut bus, &ctl, sim_kernel::Tick(1000));
        let mut tsr = [0u8; 2];
        bus.read(0xF01A0, &mut tsr).unwrap();
        assert_eq!(tsr[0] & 1, 1);
    }

    #[test]
    fn sau_uart_tx_emits_byte_after_frame_time() {
        let ctl = EventCtl::new();
        let (part, mut bus) = g23_part(ctl.clone());
        bus.write(0xF0118, &[0x04, 0x80]).unwrap();
        bus.write(0xF012A, &[0x01, 0x00]).unwrap();
        bus.write(0xF0122, &[0x01, 0x00]).unwrap();
        bus.write(0xFFF10, &[b'A', 0x00]).unwrap();
        pump(&mut bus, &ctl, sim_kernel::Tick(0));
        assert!(part.core.uart_tx.lock().unwrap().bytes().is_empty());
        pump(&mut bus, &ctl, sim_kernel::Tick(624));
        assert!(part.core.uart_tx.lock().unwrap().bytes().is_empty());
        pump(&mut bus, &ctl, sim_kernel::Tick(625));
        assert_eq!(part.core.uart_tx.lock().unwrap().bytes(), b"A");
    }

    #[test]
    fn irq_reset_masks_if_until_mk_cleared() {
        let ctl = EventCtl::new();
        let (part, mut bus) = g23_part(ctl);
        let mut mk0 = [0u8; 2];
        bus.read(0xFFFE4, &mut mk0).unwrap();
        assert_eq!(mk0, [0xFF, 0xFF]);
        bus.write(0xFFFE0, &[0x00, 0x40]).unwrap();
        assert!(part.core.irq.lock().unwrap().is_flag_set(IrqId::INTTM00));
        assert!(part.core.irq.lock().unwrap().pending().is_none());
        bus.write(0xFFFE4, &[0xFF, 0xBF]).unwrap();
        let pending = part.core.irq.lock().unwrap().pending().unwrap();
        assert_eq!(pending.index, IrqId::INTTM00);
    }

    #[test]
    fn tau_interval_latches_inttm00() {
        let ctl = EventCtl::new();
        let (part, mut bus) = g23_part(ctl.clone());
        bus.write(0xFFF18, &[31, 0]).unwrap();
        bus.write(0xF01B2, &[0x01, 0x00]).unwrap();
        pump(&mut bus, &ctl, sim_kernel::Tick(0));
        pump(&mut bus, &ctl, sim_kernel::Tick(1000));
        let mut if0 = [0u8; 2];
        bus.read(0xFFFE0, &mut if0).unwrap();
        assert_eq!(if0[1] & 0x40, 0x40);
        assert!(part.core.irq.lock().unwrap().is_flag_set(IrqId::INTTM00));
    }

    #[test]
    fn sau_uart_tx_latches_intst0() {
        let ctl = EventCtl::new();
        let (part, mut bus) = g23_part(ctl.clone());
        bus.write(0xF0118, &[0x04, 0x80]).unwrap();
        bus.write(0xF012A, &[0x01, 0x00]).unwrap();
        bus.write(0xF0122, &[0x01, 0x00]).unwrap();
        bus.write(0xFFF10, &[b'A', 0x00]).unwrap();
        pump(&mut bus, &ctl, sim_kernel::Tick(0));
        pump(&mut bus, &ctl, sim_kernel::Tick(625));
        assert!(part.core.irq.lock().unwrap().is_flag_set(IrqId::INTST0));
        let mut if0 = [0u8; 2];
        bus.read(0xFFFE0, &mut if0).unwrap();
        assert_eq!(if0[1] & 0x20, 0x20);
    }
}
