//! RL78/G23 on-chip MMIO: each peripheral window is a bus region (Renode-style).

use std::sync::{Arc, Mutex};

use sim_kernel::{
    Addr, EventCtl, HasMemoryMap, MapError, MemoryBus, MemoryMapBuilder, Ram, Resettable, Rom, Wire,
};

use crate::map::MemoryLayout;
use crate::peripherals::byte_capture::ByteCapture;
use crate::peripherals::clock::{
    ClockGenerator, ClockHocoMmio, ClockOscDivMmio, ClockSfrMmio, ClockTrimMmio,
};
use crate::peripherals::irq::{IrqBankMmio, IrqController, IrqEdgeMmio, IrqId};
use crate::peripherals::sau::{self, SauCtrlMmio, SauSdrMmio, SauUnit};
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
/// Flash option byte `FRQSEL` (QEMU `rom_ptr(0x000C2)`).
const OPTION_BYTE_ADDR: Addr = 0x000C2;

/// Board net: sink-side instances live here, not inside SAU/TAU.
struct G23Wiring {
    irq: Arc<Mutex<IrqController>>,
    uart_tx: Arc<Mutex<ByteCapture>>,
}

const SAU_IRQ: [IrqId; sau::CHANNELS] =
    [IrqId::INTST0, IrqId::INTSR0, IrqId::INTST1, IrqId::INTSR1];

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

impl G23Wiring {
    fn attach_sau(&self, sau: &mut SauUnit) {
        for (ch, id) in SAU_IRQ.iter().copied().enumerate() {
            let irq = Arc::clone(&self.irq);
            let _wire = Wire::new().source(sau.irq_source(ch)).sink(move |_, _| {
                irq.lock().expect("irq").raise(id);
            });
        }
        let uart_tx = Arc::clone(&self.uart_tx);
        let _tx = Wire::new()
            .source(sau.tx_source())
            .sink(move |values, changed| {
                if let Some(&b) = values.get(changed) {
                    uart_tx.lock().expect("tx").push(b);
                }
            });
    }

    fn attach_tau(&self, tau: &mut TauUnit) {
        for (ch, id) in TAU_IRQ.iter().copied().enumerate() {
            let irq = Arc::clone(&self.irq);
            let _wire = Wire::new().source(tau.irq_source(ch)).sink(move |_, _| {
                irq.lock().expect("irq").raise(id);
            });
        }
    }
}

/// Generic G23 core (clock / SAU0 / TAU0 / IRQ). Flash/RAM sizes come from the part.
pub struct Rl78G23Core {
    pub clock: Arc<Mutex<ClockGenerator>>,
    pub sau: Arc<Mutex<SauUnit>>,
    pub tau: Arc<Mutex<TauUnit>>,
    pub irq: Arc<Mutex<IrqController>>,
    pub uart_tx: Arc<Mutex<ByteCapture>>,
}

impl Rl78G23Core {
    #[must_use]
    pub fn new(ctl: EventCtl) -> Self {
        let clock = Arc::new(Mutex::new(ClockGenerator::new()));
        let outputs = clock.lock().expect("clock").outputs();
        let irq = Arc::new(Mutex::new(IrqController::new()));
        let uart_tx = Arc::new(Mutex::new(ByteCapture::default()));
        let wiring = G23Wiring {
            irq: Arc::clone(&irq),
            uart_tx: Arc::clone(&uart_tx),
        };
        let mut sau_unit = SauUnit::new(ctl.clone(), outputs.clone());
        let mut tau_unit = TauUnit::new(ctl, outputs);
        wiring.attach_sau(&mut sau_unit);
        wiring.attach_tau(&mut tau_unit);
        Self {
            clock,
            sau: Arc::new(Mutex::new(sau_unit)),
            tau: Arc::new(Mutex::new(tau_unit)),
            irq,
            uart_tx,
        }
    }
}

impl Resettable for Rl78G23Core {
    fn reset(&mut self) {
        Resettable::reset(&mut *self.clock.lock().expect("clock"));
        Resettable::reset(&mut *self.sau.lock().expect("sau"));
        Resettable::reset(&mut *self.tau.lock().expect("tau"));
        Resettable::reset(&mut *self.irq.lock().expect("irq"));
        self.uart_tx.lock().expect("tx").clear();
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
            .map(IRQ_EDGE, Box::new(IrqEdgeMmio::new(Arc::clone(&self.irq))))
    }
}

/// R7F100GxL: G23 core plus this part's ROM/RAM windows.
pub struct R7F100Gxl {
    pub core: Rl78G23Core,
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
        Self {
            core: Rl78G23Core::new(ctl),
        }
    }

    /// Hold-reset. `FRQSEL` is latched from ROM at [`OPTION_BYTE_ADDR`].
    pub fn reset(&mut self, bus: &mut MemoryBus) {
        let mut byte = [0u8; 1];
        let option = bus.read(OPTION_BYTE_ADDR, &mut byte).ok().map(|_| byte[0]);
        self.core
            .clock
            .lock()
            .expect("clock")
            .set_option_byte(option);
        Resettable::reset(&mut self.core);
    }
}

impl HasMemoryMap for R7F100Gxl {
    fn memory_map(&self) -> Result<MemoryMapBuilder, MapError> {
        let layout = Self::memory_layout();
        self.core.memory_map()?.merge(
            MemoryMapBuilder::new()
                .map(layout.rom_base, Box::new(Rom::erased(layout.rom_size)))?
                .map(layout.ram_base, Box::new(Ram::new(layout.ram_size)))?,
        )
    }
}
