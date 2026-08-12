//! Virtual simulation clock in nanoseconds.
//!
//! Same role as QEMU's virtual clock: peripheral timers and other deadline work
//! schedule against it (cf. `timer_init_ns`). CPU progress is converted with
//! [`crate::SimConfig::ns_per_instruction`] so instruction retirement and
//! nanosecond timers share one timeline.

use std::fmt;
use std::ops::{Add, AddAssign};

/// Monotonic virtual time in nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tick(pub u64);

impl Tick {
    pub const ZERO: Self = Self(0);
    pub const MAX: Self = Self(u64::MAX);

    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// Remaining time until a deadline (used by the quantum loop).
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    #[must_use]
    pub const fn min(self, other: Self) -> Self {
        if self.0 <= other.0 { self } else { other }
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl Add for Tick {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        self.saturating_add(rhs)
    }
}

impl AddAssign for Tick {
    fn add_assign(&mut self, rhs: Self) {
        *self = self.saturating_add(rhs);
    }
}

impl fmt::Display for Tick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ns", self.0)
    }
}

/// Simulation virtual clock. Only the simulation thread should advance it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VirtualClock {
    now: Tick,
}

impl VirtualClock {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn now(&self) -> Tick {
        self.now
    }

    pub fn advance(&mut self, delta: Tick) {
        self.now += delta;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_monotonically() {
        let mut clock = VirtualClock::new();
        assert_eq!(clock.now(), Tick::ZERO);
        clock.advance(Tick(3));
        clock.advance(Tick(4));
        assert_eq!(clock.now(), Tick(7));
    }

    #[test]
    fn add_and_sub_saturate() {
        assert_eq!(Tick::MAX + Tick(1), Tick::MAX);
        assert_eq!(Tick(2).saturating_sub(Tick(5)), Tick::ZERO);
        assert_eq!(Tick(10).min(Tick(3)), Tick(3));
        assert!(Tick::ZERO.is_zero());
    }
}
