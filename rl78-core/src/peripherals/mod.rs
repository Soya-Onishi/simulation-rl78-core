//! On-chip RL78 peripherals (clock, SAU, TAU) for the G23-class map.

mod clock;
mod g23;
mod sau;
mod tau;

pub use clock::{ClockGenerator, ClockTree};
pub use g23::{R7F100Gxl, Rl78G23Core};
pub use sau::SauUnit;
pub use tau::TauUnit;
