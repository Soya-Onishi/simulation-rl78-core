//! Probe thread: idle until shutdown (#147 Phase 2; live aggregation is Phase 4).

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const PROBE_PERIOD: Duration = Duration::from_millis(100);

pub(crate) fn probe_loop(shutdown: &AtomicBool) {
    while !shutdown.load(Ordering::Acquire) {
        thread::sleep(PROBE_PERIOD);
    }
}
