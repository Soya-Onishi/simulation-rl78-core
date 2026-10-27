//! Assembled simulation target: CPU + bus + clock + events.

use std::collections::HashMap;
use std::sync::MutexGuard;

use crate::action::{ActionCatalogEntry, ActionFailure, Device, DeviceError};
use crate::breakpoint::BreakpointStore;
use crate::bus::{Addr, BusError, MemoryBus};
use crate::clock::{Tick, VirtualClock};
use crate::command::{Command, InspectResult, Response, SimError};
use crate::cpu::{Core, Cpu, FirmwareError, RegId};
use crate::entity::{AdvanceCtx, Entity, EntityPoll};
use crate::event::{EventCtl, EventCtx, EventQueue};
use crate::stop::StopReason;

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

    fn fire_due_events(&mut self) -> Option<StopReason> {
        loop {
            let now = self.clock.now();
            let Some((_id, mut event)) = self.events_mut().pop_due(now) else {
                break;
            };
            let stop = {
                let mut ctx = EventCtx {
                    now,
                    bus: self.bus_mut(),
                    stop: None,
                };
                event.fire(&mut ctx);
                ctx.stop
            };
            if let Some(stop) = stop {
                return Some(stop);
            }
        }
        None
    }

    fn sync_breakpoints(&mut self) {
        let (cpu, _, breakpoints) = self.parts_mut();
        let snapshot = breakpoints.as_slice().to_vec();
        cpu.sync_breakpoints(&snapshot);
    }

    fn inspect_command(&mut self, cmd: Command) -> Response {
        match cmd {
            Command::ReadReg { id } => match self.read_reg(id) {
                Ok(value) => Response::Inspect(InspectResult::Reg { id, value }),
                Err(err) => Response::Error(err),
            },
            Command::WriteReg { id, value } => match self.write_reg(id, value) {
                Ok(()) => Response::Inspect(InspectResult::Ok),
                Err(err) => Response::Error(err),
            },
            Command::ReadMem { addr, len } => {
                let mut data = vec![0u8; len as usize];
                match self.read_mem(addr, &mut data) {
                    Ok(()) => Response::Inspect(InspectResult::Mem { addr, data }),
                    Err(err) => Response::Error(SimError::Bus(err)),
                }
            }
            Command::WriteMem { addr, data } => match self.write_mem(addr, &data) {
                Ok(()) => Response::Inspect(InspectResult::Ok),
                Err(err) => Response::Error(SimError::Bus(err)),
            },
            Command::AddBreakpoint { addr } => {
                let id = self.breakpoints_mut().insert(addr);
                self.sync_breakpoints();
                Response::Inspect(InspectResult::Breakpoint { id })
            }
            Command::RemoveBreakpoint { id } => {
                if self.breakpoints_mut().remove(id) {
                    self.sync_breakpoints();
                    Response::Inspect(InspectResult::Ok)
                } else {
                    Response::Error(SimError::UnknownBreakpoint(id))
                }
            }
            Command::ClearBreakpoints => {
                self.breakpoints_mut().clear();
                self.sync_breakpoints();
                Response::Inspect(InspectResult::Ok)
            }
            Command::Start
            | Command::Step
            | Command::Stop
            | Command::Quit
            | Command::SetAllowed { .. }
            | Command::NotifyHalt { .. } => {
                unreachable!("lifecycle commands are handled in Simulator::command")
            }
        }
    }
}

impl<C: Cpu> Entity for Machine<C> {
    fn poll(&mut self, ctx: AdvanceCtx) -> EntityPoll {
        if let Some(stop) = self.fire_due_events() {
            return EntityPoll::Done(Response::Stopped(stop));
        }

        let now = self.clock().now();
        if ctx.budget.is_zero() {
            return EntityPoll::NoProgress;
        }

        let budget_ns = self
            .events_mut()
            .next_deadline()
            .unwrap_or(Tick::MAX)
            .saturating_sub(now)
            .min(ctx.max_quantum)
            .min(ctx.budget);

        let Some(ns_per_insn) = self
            .cpu()
            .instruction_period()
            .filter(|tick| !tick.is_zero())
        else {
            if budget_ns.is_zero() {
                return EntityPoll::NoProgress;
            }
            self.advance_clock(budget_ns);
            if let Some(stop) = self.fire_due_events() {
                return EntityPoll::Done(Response::Stopped(stop));
            }
            return EntityPoll::Progressed;
        };
        if ns_per_insn > ctx.max_quantum {
            return EntityPoll::Done(Response::Error(SimError::InstructionPeriodExceedsQuantum {
                period: ns_per_insn,
                max_quantum: ctx.max_quantum,
            }));
        }
        let max_instructions = if ctx.step_once {
            1
        } else {
            budget_ns
                .saturating_div(ns_per_insn)
                .min(u64::from(u32::MAX)) as u32
        };
        if max_instructions == 0 {
            if budget_ns.is_zero() {
                return EntityPoll::NoProgress;
            }
            self.advance_clock(budget_ns);
            if let Some(stop) = self.fire_due_events() {
                return EntityPoll::Done(Response::Stopped(stop));
            }
            return EntityPoll::Progressed;
        }

        let result = {
            let (cpu, _, _) = self.parts_mut();
            cpu.run_quantum(max_instructions)
        };
        if result.instructions == 0 && result.stop.is_none() {
            return EntityPoll::NoProgress;
        }
        let elapsed = ns_per_insn.saturating_mul(result.instructions);
        self.advance_clock(elapsed);

        if let Some(access) = self.bus_mut().take_trap() {
            return EntityPoll::Done(Response::Stopped(StopReason::Unmapped {
                addr: access.addr,
                write: access.write,
            }));
        }

        if let Some(stop) = result.stop {
            return EntityPoll::Done(Response::Stopped(stop));
        }

        if let Some(stop) = self.fire_due_events() {
            return EntityPoll::Done(Response::Stopped(stop));
        }
        EntityPoll::Progressed
    }

    fn reset(&mut self) {
        Machine::reset(self);
    }

    fn now(&self) -> Tick {
        self.clock().now()
    }

    fn inspect(&mut self, cmd: Command) -> Response {
        self.inspect_command(cmd)
    }

    fn load_firmware(&mut self, image: &[u8]) -> Result<(), FirmwareError> {
        Machine::load_firmware(self, image)
    }

    fn action_catalog(&self) -> Vec<ActionCatalogEntry> {
        Machine::action_catalog(self)
    }

    fn poll_actions(&mut self, now: Tick) {
        Machine::poll_actions(self, now);
    }

    fn apply_action(
        &mut self,
        device_hash: u64,
        action_hash: u64,
        now: Tick,
        raw: &[(u64, f64)],
    ) -> Result<(), ActionFailure> {
        Machine::apply_action(self, device_hash, action_hash, now, raw)
    }
}
