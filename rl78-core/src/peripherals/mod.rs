//! On-chip RL78 peripherals (clock, SAU, TAU, IRQ) for the G23-class map.

mod clock;
mod g23;
pub(crate) mod irq;
mod sau;
mod tau;

pub use clock::{ClockGenerator, ClockOutputs, ClockTree, Cycles, Hertz};
pub use g23::{R7F100Gxl, Rl78G23Core};
pub use irq::{IrqController, IrqId, IrqRequest, IrqSink};
pub use sau::SauUnit;
pub use tau::TauUnit;
