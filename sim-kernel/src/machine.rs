//! Assembled simulation target: CPU + bus + clock + events.

use crate::breakpoint::BreakpointStore;
use crate::bus::{Addr, BusError, MemoryBus};
use crate::clock::VirtualClock;
use crate::command::SimError;
use crate::cpu::{Cpu, RegId};
use crate::event::EventQueue;

/// Concrete machine owned exclusively by the simulation thread.
///
/// The memory map is finished before construction: pass a [`MemoryBus`] built
/// with [`crate::MemoryMapBuilder`], rather than mutating regions afterward.
/// The bus is heap-allocated so its address stays stable across `Machine` moves
/// after the one-time [`Cpu::bind_memory`] at construction.
pub struct Machine<C: Cpu> {
    cpu: C,
    bus: Box<MemoryBus>,
    clock: VirtualClock,
    events: EventQueue,
    breakpoints: BreakpointStore,
}

impl<C: Cpu> Machine<C> {
    #[must_use]
    pub fn new(mut cpu: C, bus: MemoryBus) -> Self {
        let mut bus = Box::new(bus);
        cpu.bind_memory(bus.as_mut());
        Self {
            cpu,
            bus,
            clock: VirtualClock::new(),
            events: EventQueue::new(),
            breakpoints: BreakpointStore::new(),
        }
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
    pub fn clock(&self) -> &VirtualClock {
        &self.clock
    }

    pub fn clock_mut(&mut self) -> &mut VirtualClock {
        &mut self.clock
    }

    pub fn events_mut(&mut self) -> &mut EventQueue {
        &mut self.events
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

    pub(crate) fn parts_mut(
        &mut self,
    ) -> (
        &mut C,
        &mut MemoryBus,
        &mut VirtualClock,
        &mut EventQueue,
        &mut BreakpointStore,
    ) {
        (
            &mut self.cpu,
            &mut self.bus,
            &mut self.clock,
            &mut self.events,
            &mut self.breakpoints,
        )
    }
}
