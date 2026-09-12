//! B-lite receive path: SHM → process-local lock-free mailbox (sim thread drains).
//!
//! The receive thread must not touch board `Wire` / peripherals. The simulation
//! thread drains [`UartInbox`] at quantum entry and calls into
//! `ExternalSource::receive` (wired by the node binary later).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_queue::ArrayQueue;

use crate::shm_uart::{ShmUartError, ShmUartFrame, UartShmEndpoint};

/// Process-local UART receive mailbox (lock-free).
#[derive(Debug)]
pub struct UartInbox {
    queue: ArrayQueue<ShmUartFrame>,
    overflow_warns: AtomicBool,
}

impl UartInbox {
    #[must_use]
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            queue: ArrayQueue::new(capacity.max(1)),
            overflow_warns: AtomicBool::new(false),
        })
    }

    /// Push from the receive thread. Drops oldest if full and sets a warn latch.
    pub fn push(&self, frame: ShmUartFrame) {
        if self.queue.push(frame).is_err() {
            let _ = self.queue.force_push(frame);
            self.overflow_warns.store(true, Ordering::Relaxed);
        }
    }

    /// Drain up to `limit` frames for the simulation thread.
    pub fn drain(&self, limit: usize) -> Vec<ShmUartFrame> {
        let mut out = Vec::with_capacity(limit.min(self.queue.len()));
        for _ in 0..limit {
            match self.queue.pop() {
                Some(f) => out.push(f),
                None => break,
            }
        }
        out
    }

    /// Take-and-clear the inbox overflow warning latch.
    pub fn take_overflow_warning(&self) -> bool {
        self.overflow_warns.swap(false, Ordering::Relaxed)
    }
}

/// Background SHM → inbox copy loop.
pub struct UartRxThread {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl UartRxThread {
    /// Spawn a receiver that opens `flink`, waits on SHM Event, and copies into `inbox`.
    ///
    /// The endpoint is opened inside the worker thread because `Shmem` is not `Send`.
    pub fn spawn(
        flink: impl Into<PathBuf>,
        inbox: Arc<UartInbox>,
        edge_id: String,
    ) -> Result<Self, ShmUartError> {
        let flink = flink.into();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name(format!("uart-rx-{edge_id}"))
            .spawn(move || rx_loop(&flink, inbox, &edge_id, &stop2))
            .map_err(|e| ShmUartError::Message(e.to_string()))?;
        Ok(Self {
            stop,
            join: Some(join),
        })
    }

    /// Request stop and join.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

fn rx_loop(flink: &Path, inbox: Arc<UartInbox>, edge_id: &str, stop: &AtomicBool) {
    let endpoint = match UartShmEndpoint::open(flink) {
        Ok(ep) => ep,
        Err(err) => {
            eprintln!("uart-rx[{edge_id}]: open failed: {err}");
            return;
        }
    };
    // `drops()` is cumulative; only latch when the counter increases.
    let mut seen_drops = 0_u64;
    while !stop.load(Ordering::Relaxed) {
        match endpoint.wait(Duration::from_millis(50)) {
            Ok(()) => {}
            Err(_) => {
                // timeout or transient; keep polling until stop
            }
        }
        loop {
            match endpoint.try_pop() {
                Ok(Some(frame)) => {
                    let drops = endpoint.drops();
                    if drops > seen_drops {
                        inbox.overflow_warns.store(true, Ordering::Relaxed);
                        seen_drops = drops;
                    }
                    inbox.push(frame);
                }
                Ok(None) => break,
                Err(err) => {
                    eprintln!("uart-rx[{edge_id}]: pop error: {err}");
                    break;
                }
            }
        }
    }
}

impl Drop for UartRxThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm_uart::UartShmOwner;
    use std::time::Duration;

    #[test]
    fn blit_copies_shm_into_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let flink = dir.path().join("blit.shm");
        let owner = UartShmOwner::create(&flink, 16).unwrap();
        let prod = UartShmEndpoint::open(owner.flink()).unwrap();
        let inbox = UartInbox::new(32);
        let rx = UartRxThread::spawn(owner.flink(), Arc::clone(&inbox), "e0".into()).unwrap();

        let frame = ShmUartFrame {
            data: 0x5A,
            data_bits: 8,
            ..ShmUartFrame::default()
        };
        prod.push(frame).unwrap();
        thread::sleep(Duration::from_millis(100));
        let got = inbox.drain(8);
        assert_eq!(got, vec![frame]);
        rx.stop();
    }

    #[test]
    fn overflow_warning_latches_only_on_new_drops() {
        let dir = tempfile::tempdir().unwrap();
        let flink = dir.path().join("blit_ovf.shm");
        let owner = UartShmOwner::create(&flink, 4).unwrap();
        let prod = UartShmEndpoint::open(owner.flink()).unwrap();

        // Overflow the ring before the RX thread attaches so drops are visible.
        for i in 0..4 {
            let _ = prod.push(ShmUartFrame {
                data: i,
                ..ShmUartFrame::default()
            });
        }
        assert!(prod.drops() >= 1);

        let inbox = UartInbox::new(32);
        let rx = UartRxThread::spawn(owner.flink(), Arc::clone(&inbox), "e0".into()).unwrap();
        thread::sleep(Duration::from_millis(100));
        let _ = inbox.drain(8);
        assert!(inbox.take_overflow_warning());
        assert!(!inbox.take_overflow_warning());

        // Further frames without additional drops must not re-latch.
        prod.push(ShmUartFrame {
            data: 9,
            ..ShmUartFrame::default()
        })
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        let _ = inbox.drain(8);
        assert!(!inbox.take_overflow_warning());
        rx.stop();
    }
}
