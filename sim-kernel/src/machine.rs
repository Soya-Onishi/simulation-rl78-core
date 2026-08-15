//! Assembled simulation target: CPU + bus + clock + events.

use std::sync::MutexGuard;

use crate::breakpoint::BreakpointStore;
use crate::bus::{Addr, BusError, MemoryBus};
use crate::clock::{Tick, VirtualClock};
use crate::command::SimError;
use crate::cpu::{Cpu, RegId};
use crate::event::{EventCtl, EventQueue};
use crate::wiring::Interconnect;

/// Concrete machine owned exclusively by the simulation thread.
///
/// The memory map is finished before construction: pass a [`MemoryBus`] built
/// with [`crate::MemoryMapBuilder`], rather than mutating regions afterward.
/// The bus is heap-allocated so its address stays stable across `Machine` moves
/// after the one-time [`Cpu::bind_memory`] at construction.
///
/// The pin interconnect is likewise finished first ([`crate::WiringBuilder`]).
/// [`Machine::new`] installs an empty [`Interconnect`].
pub struct Machine<C: Cpu> {
    cpu: C,
    bus: Box<MemoryBus>,
    interconnect: Interconnect,
    clock: VirtualClock,
    ctl: EventCtl,
    breakpoints: BreakpointStore,
}

impl<C: Cpu> Machine<C> {
    /// `ctl` must be the same [`EventCtl`] clone given to peripherals (`Arc` queue).
    #[must_use]
    pub fn new(cpu: C, bus: MemoryBus, ctl: EventCtl) -> Self {
        Self::new_with_interconnect(cpu, bus, ctl, Interconnect::empty())
    }

    /// Same as [`Self::new`] with a finished [`Interconnect`].
    #[must_use]
    pub fn new_with_interconnect(
        mut cpu: C,
        bus: MemoryBus,
        ctl: EventCtl,
        interconnect: Interconnect,
    ) -> Self {
        let mut bus = Box::new(bus);
        cpu.bind_memory(bus.as_mut());
        ctl.set_now(Tick::ZERO);
        Self {
            cpu,
            bus,
            interconnect,
            clock: VirtualClock::new(),
            ctl,
            breakpoints: BreakpointStore::new(),
        }
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

    #[must_use]
    pub fn interconnect(&self) -> &Interconnect {
        &self.interconnect
    }

    pub fn interconnect_mut(&mut self) -> &mut Interconnect {
        &mut self.interconnect
    }

    pub fn bus_and_interconnect_mut(&mut self) -> (&mut MemoryBus, &mut Interconnect) {
        (&mut self.bus, &mut self.interconnect)
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
        self.cpu.read_reg(id)
    }

    pub fn write_reg(&mut self, id: RegId, value: u64) -> Result<(), SimError> {
        self.cpu.write_reg(id, value)
    }

    pub fn read_mem(&mut self, addr: Addr, buf: &mut [u8]) -> Result<(), BusError> {
        self.bus.read(addr, buf)
    }

    pub fn write_mem(&mut self, addr: Addr, buf: &[u8]) -> Result<(), BusError> {
        self.bus.write(addr, buf)
    }

    pub(crate) fn parts_mut(&mut self) -> (&mut C, &mut MemoryBus, &mut BreakpointStore) {
        (&mut self.cpu, &mut self.bus, &mut self.breakpoints)
    }
}
