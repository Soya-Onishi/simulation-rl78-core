//! Assembled simulation target: CPU + bus + clock + events.

use std::collections::HashMap;
use std::sync::MutexGuard;

use crate::action::{ActionCatalogEntry, ActionFailure, Device, DeviceError};
use crate::breakpoint::BreakpointStore;
use crate::bus::{Addr, BusError, MemoryBus};
use crate::clock::{Tick, VirtualClock};
use crate::command::SimError;
use crate::cpu::{Core, Cpu, FirmwareError, RegId};
use crate::event::{EventCtl, EventQueue};

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
    /// Devices keyed by [`crate::id_hash`] of the device name.
    devices: HashMap<u64, Box<dyn Device>>,
    /// Registration order. Keys into [`Self::devices`], used only when order matters.
    device_order: Vec<u64>,
    clock: VirtualClock,
    ctl: EventCtl,
    breakpoints: BreakpointStore,
}

impl<C: Cpu> Machine<C> {
    /// `ctl` must be the same [`EventCtl`] clone given to peripherals (`Arc` queue).
    #[must_use]
    pub fn new(cpu: C, bus: MemoryBus, ctl: EventCtl) -> Self {
        Self::with_devices(cpu, bus, ctl, Vec::new()).expect("empty device list")
    }

    /// Like [`Self::new`], and retains `devices` for [`Self::reset`].
    ///
    /// # Errors
    ///
    /// Two devices hash to the same [`crate::id_hash`] value.
    pub fn with_devices(
        mut cpu: C,
        bus: MemoryBus,
        ctl: EventCtl,
        devices: Vec<Box<dyn Device>>,
    ) -> Result<Self, DeviceError> {
        let mut device_order = Vec::with_capacity(devices.len());
        let mut by_hash = HashMap::with_capacity(devices.len());
        for device in devices {
            let hash = device.name_hash();
            let name = device.name().to_string();
            if by_hash.insert(hash, device).is_some() {
                return Err(DeviceError::DuplicateDevice { name });
            }
            device_order.push(hash);
        }
        let mut bus = Box::new(bus);
        cpu.core_mut().bind_memory(bus.as_mut());
        ctl.set_now(Tick::ZERO);
        Ok(Self {
            cpu,
            bus,
            devices: by_hash,
            device_order,
            clock: VirtualClock::new(),
            ctl,
            breakpoints: BreakpointStore::new(),
        })
    }

    /// Device registered under `name_hash` (`xxh3_64` of its name).
    pub fn device_mut(&mut self, name_hash: u64) -> Option<&mut dyn Device> {
        Some(self.devices.get_mut(&name_hash)?.as_mut())
    }

    /// Release action effects whose virtual-time deadline has arrived.
    pub fn poll_actions(&mut self, now: Tick) {
        for device in self.devices.values_mut() {
            device.poll(now);
        }
    }

    /// Apply one action. Expiring deadlines is [`Self::poll_actions`], done by the caller first.
    pub fn apply_action(
        &mut self,
        device_hash: u64,
        action_hash: u64,
        now: Tick,
        raw: &[(u64, f64)],
    ) -> Result<(), ActionFailure> {
        let Some(device) = self.device_mut(device_hash) else {
            return Err(ActionFailure::NotFoundDevice);
        };
        device.apply(action_hash, now, raw)
    }

    /// Actions currently registered, in device registration order.
    #[must_use]
    pub fn action_catalog(&self) -> Vec<ActionCatalogEntry> {
        let mut out = Vec::new();
        for hash in &self.device_order {
            let device = &self.devices[hash];
            for spec in device.action_specs() {
                out.push(ActionCatalogEntry {
                    device: device.name().to_string(),
                    device_hash: device.name_hash(),
                    action: spec.id.clone(),
                    action_hash: spec.hash,
                    description: spec.description.clone(),
                    params: spec.params.clone(),
                });
            }
        }
        out
    }

    /// Hold-reset all devices, then the CPU. Call after firmware is in ROM.
    pub fn reset(&mut self) {
        for device in self.devices.values_mut() {
            device.reset(&mut self.bus);
        }
        self.cpu.reset(&mut self.bus);
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
