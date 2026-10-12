//! Process-global virtual time for sinks that do not hold an [`crate::EventCtl`].
//!
//! Updated whenever [`crate::EventCtl::set_now`] runs (simulation thread). Probe
//! taps and similar observers read [`virtual_time_ns`] without locking.

use std::sync::atomic::{AtomicU64, Ordering};

static GLOBAL_VIRTUAL_TIME_NS: AtomicU64 = AtomicU64::new(0);

/// Current virtual time in nanoseconds (process-global mirror of the active clock).
#[must_use]
pub fn virtual_time_ns() -> u64 {
    GLOBAL_VIRTUAL_TIME_NS.load(Ordering::Relaxed)
}

/// Store the process-global virtual time (called from [`crate::EventCtl::set_now`]).
pub fn set_global_virtual_time_ns(ns: u64) {
    GLOBAL_VIRTUAL_TIME_NS.store(ns, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_vt_round_trip() {
        set_global_virtual_time_ns(42);
        assert_eq!(virtual_time_ns(), 42);
        set_global_virtual_time_ns(0);
    }
}
