//! On-chip RL78 peripherals (clock, SAU, TAU, IRQ) and sim-only semihosting.

mod clock;
mod g23;
pub(crate) mod irq;
mod sau;
mod semihosting;
mod tau;

pub use clock::{ClockGenerator, ClockOutputs, ClockTree, Cycles, Hertz};
pub use g23::{R7F100Gxl, Rl78G23Core};
pub use irq::{IrqController, IrqId, IrqRequest};
pub use sau::SauUnit;
pub use semihosting::SemihostingUnit;
pub use tau::TauUnit;
