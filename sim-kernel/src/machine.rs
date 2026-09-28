//! Assembled simulation target: CPU + bus + clock + events.

use std::sync::MutexGuard;

use crate::breakpoint::BreakpointStore;
use crate::bus::{Addr, BusError, MemoryBus};
use crate::clock::{Tick, VirtualClock};
use crate::command::SimError;
use crate::cpu::{Core, Cpu, FirmwareError, RegId};
use crate::event::{EventCtl, EventQueue};
use crate::reset::Device;

/// Concrete machine owned exclusively by the simulation thread.
///
/// The memory map is finished before construction: pass a [`MemoryBus`] built
/// with [`crate::MemoryMapBuilder`], rather than mutating regions afterward.
/// The bus is heap-allocated so its address stays stable across `Machine` moves
/// after the one-time [`Core::bind_memory`] at construction.
///
/// `cpu` is the SoC ([`Cpu`]): it owns the instruction [`Core`] and, for a clocked
/// part, the frequency used to size quanta. Extra [`Device`]s are board peripherals
/// kept separately from the bus so [`Self::reset`] can hold-reset each one once,
/// even when its MMIO is split across multiple mapped regions.
pub struct Machine<C: Cpu> {
    cpu: C,
    bus: Box<MemoryBus>,
    devices: Vec<Box<dyn Device>>,
    clock: VirtualClock,
    ctl: EventCtl,
    breakpoints: BreakpointStore,
}

impl<C: Cpu> Machine<C> {
    /// `ctl` must be the same [`EventCtl`] clone given to peripherals (`Arc` queue).
    #[must_use]
    pub fn new(cpu: C, bus: MemoryBus, ctl: EventCtl) -> Self {
        Self::with_devices(cpu, bus, ctl, Vec::new())
    }

    /// Like [`Self::new`], and retains `devices` for [`Self::reset`].
    #[must_use]
    pub fn with_devices(
        mut cpu: C,
        bus: MemoryBus,
        ctl: EventCtl,
        devices: Vec<Box<dyn Device>>,
    ) -> Self {
        let mut bus = Box::new(bus);
        cpu.core_mut().bind_memory(bus.as_mut());
        ctl.set_now(Tick::ZERO);
        Self {
            cpu,
            bus,
            devices,
            clock: VirtualClock::new(),
            ctl,
            breakpoints: BreakpointStore::new(),
        }
    }

    /// Hold-reset all devices, then the CPU. Call after firmware is in ROM.
    pub fn reset(&mut self) {
        for device in &mut self.devices {
            device.reset(&mut self.bus);
        }
        self.cpu.reset(&mut self.bus);
    }

    /// Move virtual time forward. A target at or behind `now` is ignored.
    pub fn set_virtual_time(&mut self, now: Tick) {
        if now <= self.clock.now() {
            return;
        }
        self.clock.set(now);
        self.ctl.set_now(now);
    }

    #[must_use]
    pub fn event_ctl(&self) -> &EventCtl {
        &self.ctl
    }

    #[must_use]
    pub fn cpu(&self) -> &C {
        &self.cpu
    }

    pub fn cpu_mut(&mut self) -> &mut C {
        &mut self.cpu
    }

    #[must_use]
    pub fn bus(&self) -> &MemoryBus {
        &self.bus
    }

    pub fn bus_mut(&mut self) -> &mut MemoryBus {
        &mut self.bus
    }

    /// Load guest firmware bytes via [`Core::load_firmware`].
    pub fn load_firmware(&mut self, image: &[u8]) -> Result<(), FirmwareError> {
        self.cpu.core_mut().load_firmware(self.bus.as_mut(), image)
    }

    #[must_use]
    pub fn clock(&self) -> &VirtualClock {
        &self.clock
    }

    pub fn advance_clock(&mut self, delta: Tick) {
        self.clock.advance(delta);
        self.ctl.set_now(self.clock.now());
    }

    pub fn events_mut(&mut self) -> MutexGuard<'_, EventQueue> {
        self.ctl.events()
    }

    pub fn breakpoints_mut(&mut self) -> &mut BreakpointStore {
        &mut self.breakpoints
    }

    #[must_use]
    pub fn breakpoints(&self) -> &BreakpointStore {
        &self.breakpoints
    }

    pub fn read_reg(&self, id: RegId) -> Result<u64, SimError> {
        self.cpu.core().read_reg(id)
    }

    pub fn write_reg(&mut self, id: RegId, value: u64) -> Result<(), SimError> {
        self.cpu.core_mut().write_reg(id, value)
    }

    pub fn read_mem(&mut self, addr: Addr, buf: &mut [u8]) -> Result<(), BusError> {
        self.bus.read(addr, buf)
    }

    pub fn write_mem(&mut self, addr: Addr, buf: &[u8]) -> Result<(), BusError> {
        self.bus.write(addr, buf)
    }

    pub(crate) fn parts_mut(&mut self) -> (&mut C::Core, &mut MemoryBus, &mut BreakpointStore) {
        (
            self.cpu.core_mut(),
            self.bus.as_mut(),
            &mut self.breakpoints,
        )
    }
}
