//! RL78/G23 on-chip MMIO: each peripheral window is a bus region (Renode-style).

use std::sync::{Arc, Mutex};

use sim_kernel::{EventCtl, MapError, MemoryBus, MemoryMapBuilder, Ram, Rom};

use crate::map::MemoryLayout;
use crate::peripherals::clock::{ClockBank, ClockGenerator, ClockMmio};
use crate::peripherals::sau::{SauBank, SauMmio, SauUnit};
use crate::peripherals::tau::{TauBank, TauMmio, TauUnit};

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
        let sau = Arc::new(Mutex::new(SauUnit::new(ctl.clone(), Arc::clone(&clock))));
        let tau = Arc::new(Mutex::new(TauUnit::new(ctl, Arc::clone(&clock))));
        Self { clock, sau, tau }
    }

    pub fn reset(&self, option_byte: Option<u8>) {
        self.clock.lock().expect("clock").reset(option_byte);
    }

    /// Map every clock/SAU/TAU window. Missing a [`ClockBank`]/[`SauBank`]/[`TauBank`]
    /// variant in `ALL` is a compile-time hole; this loop is the runtime check.
    pub fn map_into(&self, mut builder: MemoryMapBuilder) -> Result<MemoryMapBuilder, MapError> {
        for bank in ClockBank::ALL {
            builder = builder.map(
                bank.base(),
                Box::new(ClockMmio::new(Arc::clone(&self.clock), bank)),
            )?;
        }
        for bank in SauBank::ALL {
            builder = builder.map(
                bank.base(),
                Box::new(SauMmio::new(Arc::clone(&self.sau), bank)),
            )?;
        }
        for bank in TauBank::ALL {
            builder = builder.map(
                bank.base(),
                Box::new(TauMmio::new(Arc::clone(&self.tau), bank)),
            )?;
        }
        Ok(builder)
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
        let builder = MemoryMapBuilder::new().policy(cfg.unmapped);
        let builder = core.map_into(builder)?;
        let bus = builder
            .map(layout.rom_base, Box::new(Rom::new(layout.rom_size)))?
            .map(layout.ram_base, Box::new(Ram::new(layout.ram_size)))?
            .build();
        Ok((Self { core }, bus))
    }
}
