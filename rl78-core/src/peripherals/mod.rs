//! On-chip RL78 peripherals (clock, SAU, TAU) for the G23-class map.

mod chip;
mod clock;
mod sau;
mod tau;

pub use chip::{G23Peripherals, g23_sfr_windows};
pub use clock::{ClockGenerator, ClockTree};
pub use sau::SauUnit;
pub use tau::TauUnit;
