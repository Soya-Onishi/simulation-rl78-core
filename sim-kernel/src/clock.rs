//! Virtual simulation clock in nanoseconds.
//!
//! Same role as QEMU's virtual clock: peripheral timers and other deadline work
//! schedule against it (cf. `timer_init_ns`). CPU progress is converted with
//! [`crate::SimConfig::ns_per_instruction`] so instruction retirement and
//! nanosecond timers share one timeline.
//!
//! [`Tick`] is both an **instant** (`VirtualClock::now`, event deadlines) and a
//! **duration** on the same nanosecond scale (timer periods, `max_quantum`,
//! `ns_per_instruction`). A separate `DurationNs` type is not used: durations
//! add to instants with saturating `Add` / [`Tick::saturating_add`].

use std::fmt;
use std::ops::{Add, AddAssign};

/// Nanoseconds in one SI second. Virtual time and [`Tick`] durations use this
/// scale (same as QEMU `NANOSECONDS_PER_SECOND`).
pub const NS_PER_SEC: u64 = 1_000_000_000;

/// Monotonic virtual time, or a duration, in nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tick(pub u64);

impl Tick {
    pub const ZERO: Self = Self(0);
    pub const MAX: Self = Self(u64::MAX);

    #[must_use]
    pub const fn from_ns(ns: u64) -> Self {
        Self(ns)
    }

    #[must_use]
    pub const fn as_ns(self) -> u64 {
        self.0
    }

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
    pub const fn saturating_mul(self, n: u64) -> Self {
        Self(self.0.saturating_mul(n))
    }

    /// Floor-divide two durations (`budget / ns_per_instruction`).
    ///
    /// A zero divisor yields `0` so a misconfigured quantum cannot request an
    /// unbounded instruction count.
    #[must_use]
    pub const fn saturating_div(self, rhs: Self) -> u64 {
        match self.0.checked_div(rhs.0) {
            Some(q) => q,
            None => 0,
        }
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
        assert_eq!(Tick(10).saturating_mul(3), Tick(30));
        assert_eq!(Tick(10).saturating_div(Tick(3)), 3);
        assert_eq!(Tick(10).saturating_div(Tick::ZERO), 0);
        assert_eq!(Tick::from_ns(NS_PER_SEC).as_ns(), NS_PER_SEC);
    }
}
