//! RL78/G23 on-chip MMIO: each peripheral window is a bus region (Renode-style).

use std::sync::{Arc, Mutex};

use sim_kernel::{Addr, EventCtl, HasMemoryMap, MapError, MemoryBus, MemoryMapBuilder, Ram, Rom};

use crate::map::MemoryLayout;
use crate::peripherals::clock::{
    ClockGenerator, ClockHocoMmio, ClockOscDivMmio, ClockSfrMmio, ClockTrimMmio,
};
use crate::peripherals::sau::{SauCtrlMmio, SauSdrMmio, SauUnit};
use crate::peripherals::tau::{TauCtrlMmio, TauTdrMmio, TauTisMmio, TauUnit};

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

/// Generic G23 core (clock / SAU0 / TAU0). Flash/RAM sizes come from the part.
pub struct Rl78G23Core {
    pub clock: Arc<Mutex<ClockGenerator>>,
    pub sau: Arc<Mutex<SauUnit>>,
    pub tau: Arc<Mutex<TauUnit>>,
}

impl Rl78G23Core {
    #[must_use]
    pub fn new(ctl: EventCtl) -> Self {
        let clock = Arc::new(Mutex::new(ClockGenerator::new()));
        let outputs = clock.lock().expect("clock").outputs();
        let sau = Arc::new(Mutex::new(SauUnit::new(ctl.clone(), outputs.clone())));
        let tau = Arc::new(Mutex::new(TauUnit::new(ctl, outputs)));
        Self { clock, sau, tau }
    }

    pub fn reset(&self, option_byte: Option<u8>) {
        self.clock.lock().expect("clock").reset(option_byte);
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
            .map(TAU_TIS, Box::new(TauTisMmio::new(Arc::clone(&self.tau))))
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

    pub fn build(
        cfg: &crate::G23MachineConfig,
        ctl: EventCtl,
    ) -> Result<(Self, MemoryBus), MapError> {
        let core = Rl78G23Core::new(ctl);
        core.reset(cfg.option_byte);
        let layout = Self::memory_layout();
        let bus = MemoryMapBuilder::new()
            .policy(cfg.unmapped)
            .merge(core.memory_map()?)?
            .map(layout.rom_base, Box::new(Rom::new(layout.rom_size)))?
            .map(layout.ram_base, Box::new(Ram::new(layout.ram_size)))?
            .build();
        Ok((Self { core }, bus))
    }
}

impl HasMemoryMap for R7F100Gxl {
    fn memory_map(&self) -> Result<MemoryMapBuilder, MapError> {
        let layout = Self::memory_layout();
        self.core.memory_map()?.merge(
            MemoryMapBuilder::new()
                .map(layout.rom_base, Box::new(Rom::new(layout.rom_size)))?
                .map(layout.ram_base, Box::new(Ram::new(layout.ram_size)))?,
        )
    }
}
