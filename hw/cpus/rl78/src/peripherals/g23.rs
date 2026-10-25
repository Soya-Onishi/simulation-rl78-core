//! RL78/G23 on-chip MMIO: each peripheral window is a bus region (Renode-style).

use std::sync::{Arc, Mutex};

use sim_kernel::{
    Addr, Cpu, Device, DigitalLevel, EventCtl, HasMemoryMap, MapError, MemoryBus, MemoryMapBuilder,
    Ram, Resettable, Rom, SourcePort, Tick, Wire,
};

use crate::cpu::Rl78Cpu;

use crate::map::MemoryLayout;
use crate::peripherals::SemihostingUnit;
use crate::peripherals::clock::{
    ClockGenerator, ClockHocoMmio, ClockOscDivMmio, ClockSfrMmio, ClockTrimMmio,
};
use crate::peripherals::gpio::{Gpio, GpioMmio, GpioWindow, PINS};
use crate::peripherals::irq::{IrqBankMmio, IrqController, IrqEdgeMmio, IrqId, bind_cpu_line};
use crate::peripherals::sau::{self, SauCtrlMmio, SauSdrMmio, SauUnit};
use crate::peripherals::semihosting::SemihostingMmio;
use crate::peripherals::tau::{self, TauCtrlMmio, TauTdrMmio, TauTisMmio, TauUnit};

/// G23 clock / SAU0 / TAU0 window bases (wiring, not device internals).
const CLOCK_SFR: Addr = 0xFFFA0;
const CLOCK_OSC_DIV: Addr = 0xF00F2;
const CLOCK_HOCO: Addr = 0xF00A0;
const CLOCK_TRIM: Addr = 0xF0212;
const SAU_SDR01: Addr = 0xFFF10;
const SAU_SDR23: Addr = 0xFFF44;
const SAU_CTRL: Addr = 0xF0100;
const TAU_TDR01: Addr = 0xFFF18;
const TAU_TDR27: Addr = 0xFFF64;
const TAU_CTRL: Addr = 0xF0180;
const TAU_TIS: Addr = 0xF0074;
const IRQ_IFMK0: Addr = 0xFFFE0;
const IRQ_IFMK1: Addr = 0xFFFD0;
const IRQ_EDGE: Addr = 0xFFF38;
const GPIO_LEVEL: Addr = 0xFFF00;
const GPIO_MODE: Addr = 0xFFF20;
const GPIO_OTHER0: Addr = 0xF0030;
const GPIO_PIOR: Addr = 0xF0077;
const GPIO_PIDIS: Addr = 0xF007D;
const GPIO_OTHER1: Addr = 0xF0260;
const SEMIHOSTING: Addr = 0xE0000;
/// Flash option byte `FRQSEL` (QEMU `rom_ptr(0x000C2)`).
const OPTION_BYTE_ADDR: Addr = 0x000C2;

const SAU_IRQ: [IrqId; sau::CHANNELS] =
    [IrqId::INTST0, IrqId::INTSR0, IrqId::INTST1, IrqId::INTSR1];
const SAU_ERR_IRQ: [IrqId; sau::UARTS] = [IrqId::INTSRE0, IrqId::INTSRE1];

const TAU_IRQ: [IrqId; tau::CHANNELS] = [
    IrqId::INTTM00,
    IrqId::INTTM01,
    IrqId::INTTM02,
    IrqId::INTTM03,
    IrqId::INTTM04,
    IrqId::INTTM05,
    IrqId::INTTM06,
    IrqId::INTTM07,
];

fn wiring(sau: &mut SauUnit, tau: &mut TauUnit, irq: &Arc<Mutex<IrqController>>) {
    for (ch, id) in SAU_IRQ.iter().copied().enumerate() {
        let irq = Arc::clone(irq);
        let _wire = Wire::new()
            .source(sau.irq_source(ch))
            .sink(move |values, changed| IrqController::on_input(&irq, id, values, changed));
    }
    for (uart, id) in SAU_ERR_IRQ.iter().copied().enumerate() {
        let irq = Arc::clone(irq);
        let _wire = Wire::new()
            .source(sau.err_source(uart))
            .sink(move |values, changed| IrqController::on_input(&irq, id, values, changed));
    }
    for (ch, id) in TAU_IRQ.iter().copied().enumerate() {
        let irq = Arc::clone(irq);
        let _wire = Wire::new()
            .source(tau.irq_source(ch))
            .sink(move |values, changed| IrqController::on_input(&irq, id, values, changed));
    }

    bind_cpu_line(irq.clone());
}

/// Generic G23 core (clock / SAU0 / TAU0 / GPIO / IRQ). Flash/RAM sizes come from the part.
///
/// Board-edge UART and GPIO ports are wired by the board crate, not here —
/// endpoint names belong to the board topology.
pub struct Rl78G23Core {
    pub clock: Arc<Mutex<ClockGenerator>>,
    pub sau: Arc<Mutex<SauUnit>>,
    pub tau: Arc<Mutex<TauUnit>>,
    pub gpio: Arc<Mutex<Gpio>>,
    pub semihosting: Arc<Mutex<SemihostingUnit>>,
    pub irq: Arc<Mutex<IrqController>>,
    /// RL78 instruction engine. Created in [`Self::new`]. tlib allows one live instance.
    core: Rl78Cpu,
}

impl Rl78G23Core {
    #[must_use]
    pub fn new(ctl: EventCtl) -> Self {
        let clock = Arc::new(Mutex::new(ClockGenerator::new()));
        let outputs = clock.lock().expect("clock").outputs();
        let irq = Arc::new(Mutex::new(IrqController::new()));
        let mut sau_unit = SauUnit::new(ctl.clone(), outputs.clone());
        let mut tau_unit = TauUnit::new(ctl.clone(), outputs);
        let semihosting_unit = SemihostingUnit::new(ctl);
        let core = Rl78Cpu::new();

        wiring(&mut sau_unit, &mut tau_unit, &irq);

        Self {
            clock,
            sau: Arc::new(Mutex::new(sau_unit)),
            tau: Arc::new(Mutex::new(tau_unit)),
            gpio: Arc::new(Mutex::new(Gpio::new())),
            semihosting: Arc::new(Mutex::new(semihosting_unit)),
            irq,
            core,
        }
    }
}

impl Resettable for Rl78G23Core {
    fn reset(&mut self, bus: &mut MemoryBus) {
        Resettable::reset(&mut *self.clock.lock().expect("clock"), bus);
        Resettable::reset(&mut *self.sau.lock().expect("sau"), bus);
        Resettable::reset(&mut *self.tau.lock().expect("tau"), bus);
        Resettable::reset(&mut *self.gpio.lock().expect("gpio"), bus);
        Resettable::reset(&mut *self.irq.lock().expect("irq"), bus);
        Resettable::reset(&mut *self.semihosting.lock().expect("semihosting"), bus);
        Resettable::reset(&mut self.core, bus);
    }
}

impl HasMemoryMap for Rl78G23Core {
    fn memory_map(&self) -> Result<MemoryMapBuilder, MapError> {
        MemoryMapBuilder::new()
            .map(
                CLOCK_SFR,
                Box::new(ClockSfrMmio::new(Arc::clone(&self.clock))),
            )?
            .map(
                CLOCK_OSC_DIV,
                Box::new(ClockOscDivMmio::new(Arc::clone(&self.clock))),
            )?
            .map(
                CLOCK_HOCO,
                Box::new(ClockHocoMmio::new(Arc::clone(&self.clock))),
            )?
            .map(
                CLOCK_TRIM,
                Box::new(ClockTrimMmio::new(Arc::clone(&self.clock))),
            )?
            .map(
                SAU_SDR01,
                Box::new(SauSdrMmio::new(Arc::clone(&self.sau), 0)),
            )?
            .map(
                SAU_SDR23,
                Box::new(SauSdrMmio::new(Arc::clone(&self.sau), 2)),
            )?
            .map(SAU_CTRL, Box::new(SauCtrlMmio::new(Arc::clone(&self.sau))))?
            .map(
                TAU_TDR01,
                Box::new(TauTdrMmio::new(Arc::clone(&self.tau), 0, 4)),
            )?
            .map(
                TAU_TDR27,
                Box::new(TauTdrMmio::new(Arc::clone(&self.tau), 2, 0x0C)),
            )?
            .map(TAU_CTRL, Box::new(TauCtrlMmio::new(Arc::clone(&self.tau))))?
            .map(TAU_TIS, Box::new(TauTisMmio::new(Arc::clone(&self.tau))))?
            .map(
                IRQ_IFMK0,
                Box::new(IrqBankMmio::new(Arc::clone(&self.irq), 0)),
            )?
            .map(
                IRQ_IFMK1,
                Box::new(IrqBankMmio::new(Arc::clone(&self.irq), 1)),
            )?
            .map(IRQ_EDGE, Box::new(IrqEdgeMmio::new(Arc::clone(&self.irq))))?
            .map(
                GPIO_LEVEL,
                Box::new(GpioMmio::new(Arc::clone(&self.gpio), GpioWindow::Level)),
            )?
            .map(
                GPIO_MODE,
                Box::new(GpioMmio::new(Arc::clone(&self.gpio), GpioWindow::Mode)),
            )?
            .map(
                GPIO_PIOR,
                Box::new(GpioMmio::new(
                    Arc::clone(&self.gpio),
                    GpioWindow::Redirection,
                )),
            )?
            .map(
                GPIO_PIDIS,
                Box::new(GpioMmio::new(
                    Arc::clone(&self.gpio),
                    GpioWindow::GlobalInputDisable,
                )),
            )?
            .map(
                GPIO_OTHER0,
                Box::new(GpioMmio::new(Arc::clone(&self.gpio), GpioWindow::Other0)),
            )?
            .map(
                GPIO_OTHER1,
                Box::new(GpioMmio::new(Arc::clone(&self.gpio), GpioWindow::Other1)),
            )?
            .map(
                SEMIHOSTING,
                Box::new(SemihostingMmio::new(Arc::clone(&self.semihosting))),
            )
    }
}

/// Package pins of the port block. The [`Gpio`] peripheral stays on [`Rl78G23Core`];
/// these [`SourcePort`]s are the terminals that leave the core.
struct PackagePins {
    /// External level into the pad. Wired to [`Gpio::on_pin`].
    input: [SourcePort<DigitalLevel>; PINS],
    /// Pad level driven by the port block. Shared so a later [`Wire::source`]
    /// on [`R7F100Gxl::gpio_output`] is the port the core drives.
    output: [Arc<Mutex<SourcePort<DigitalLevel>>>; PINS],
}

/// Connect each package pin to the core port block.
fn wire_package_pins(core: &Rl78G23Core) -> PackagePins {
    let mut input = std::array::from_fn(|_| SourcePort::new());
    let output = std::array::from_fn(|_| Arc::new(Mutex::new(SourcePort::new())));
    for pin in 0..PINS {
        let gpio = Arc::clone(&core.gpio);
        let _input = Wire::new()
            .source(&mut input[pin])
            .sink(move |values, changed| Gpio::on_pin(&gpio, pin, values, changed));
        let pkg_out = Arc::clone(&output[pin]);
        {
            let mut gpio = core.gpio.lock().expect("gpio");
            let _output = Wire::new()
                .source(gpio.out_source(pin))
                .sink(move |values, changed| {
                    if let Some(&level) = values.get(changed) {
                        let port = pkg_out.lock().expect("package output").clone();
                        port.drive(level);
                    }
                });
        }
    }
    PackagePins { input, output }
}

/// R7F100GxL: G23 core, this part's ROM/RAM windows, and the package pins.
pub struct R7F100Gxl {
    pub core: Rl78G23Core,
    pins: PackagePins,
}

impl R7F100Gxl {
    #[must_use]
    pub fn memory_layout() -> MemoryLayout {
        MemoryLayout {
            rom_base: 0x00000,
            rom_size: 0x20000,
            ram_base: 0xF3F00,
            ram_size: 0xC000,
        }
    }

    #[must_use]
    pub fn new(ctl: EventCtl) -> Self {
        let core = Rl78G23Core::new(ctl);
        let pins = wire_package_pins(&core);
        Self { core, pins }
    }

    /// Package input for `pin` (`0` is P0.0). Already wired into the core port block.
    #[must_use]
    pub fn gpio_input(&self, pin: usize) -> SourcePort<DigitalLevel> {
        self.pins.input[pin].clone()
    }

    /// Package output for `pin` (`0` is P0.0).
    ///
    /// Lock this and pass it to [`Wire::source`]. The core drives that same port,
    /// so the sink sees later pad levels. A second `source` replaces the wire.
    #[must_use]
    pub fn gpio_output(&self, pin: usize) -> Arc<Mutex<SourcePort<DigitalLevel>>> {
        Arc::clone(&self.pins.output[pin])
    }
}

impl Resettable for R7F100Gxl {
    /// Hold-reset. `FRQSEL` is latched from ROM at [`OPTION_BYTE_ADDR`].
    fn reset(&mut self, bus: &mut MemoryBus) {
        let mut byte = [0u8; 1];
        let option = bus.read(OPTION_BYTE_ADDR, &mut byte).ok().map(|_| byte[0]);
        self.core
            .clock
            .lock()
            .expect("clock")
            .set_option_byte(option);
        Resettable::reset(&mut self.core, bus);
    }
}

impl Cpu for R7F100Gxl {
    type Core = Rl78Cpu;

    fn core(&self) -> &Rl78Cpu {
        &self.core.core
    }

    fn core_mut(&mut self) -> &mut Rl78Cpu {
        &mut self.core.core
    }

    fn instruction_period(&self) -> Option<Tick> {
        self.core.clock.lock().expect("clock").f_clk().period()
    }
}

impl Device for R7F100Gxl {}

impl HasMemoryMap for R7F100Gxl {
    fn memory_map(&self) -> Result<MemoryMapBuilder, MapError> {
        let layout = Self::memory_layout();
        self.core.memory_map()?.merge(
            MemoryMapBuilder::new()
                .map(layout.rom_base, Box::new(Rom::erased(layout.rom_size)))?
                .map(layout.ram_base, Box::new(Ram::new(layout.ram_size)))?
                .alias(0xF3000, 0x3000, 3840)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serial_test::serial;
    use sim_kernel::{DigitalLevel, EventCtl, Wire};

    use super::*;

    #[test]
    #[serial]
    fn package_output_sink_sees_core_drive() {
        let part = R7F100Gxl::new(EventCtl::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let port = part.gpio_output(0);
        let _wire = {
            let mut port = port.lock().expect("package output");
            Wire::new().source(&mut port).sink(move |values, changed| {
                if let Some(&level) = values.get(changed) {
                    recorded.lock().expect("levels").push(level);
                }
            })
        };
        part.core
            .gpio
            .lock()
            .expect("gpio")
            .out_source(0)
            .drive(DigitalLevel::High);
        assert_eq!(
            seen.lock().expect("levels").as_slice(),
            &[DigitalLevel::High]
        );
    }
}
