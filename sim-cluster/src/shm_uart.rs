//! UART SHM ring with process-shared Event wake (one directed edge per segment).
//!
//! Layout (creator initializes; peers attach):
//! ```text
//! [Event ...][RingHeader][ShmUartFrame; capacity]
//! ```
//!
//! Single-producer / single-consumer. On overflow the oldest frame is dropped
//! and [`RingHeader::drops`] is incremented (caller should warn).
//!
//! # TODO(ipc): replace provisional wait/wake stack
//!
//! Wake uses `raw_sync` + `shared_memory` as an MVP stand-in (issue #37 example).
//! `raw_sync` is effectively unmaintained — keep call sites narrow so we can
//! swap to an actively developed alternative (thin futex wrapper, iceoryx2,
//! etc.) without rewriting the ring layout. Touch points: `Event::new` /
//! `from_existing` / `wait` / `set`, plus the `evt_bytes` prefix size.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

// TODO(ipc): drop `raw_sync` when wait/wake is replaced (see module docs).
use raw_sync::Timeout;
use raw_sync::events::{Event, EventInit, EventState};
use shared_memory::{Shmem, ShmemConf};

/// Packed UART frame carried on the SHM ring (host endian, `repr(C)`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShmUartFrame {
    pub data: u16,
    pub data_bits: u8,
    pub stop_bits: u8,
    pub parity: u8,
    pub inverted: u8,
    pub _pad: u16,
    pub bit_time_ns: u64,
}

const MAGIC: u32 = 0x5355_4152; // "SUAR"
const HEADER_ALIGN: usize = 64;

#[repr(C, align(64))]
struct RingHeader {
    magic: u32,
    capacity: u32,
    head: AtomicU32,
    tail: AtomicU32,
    drops: AtomicU64,
}

/// Errors from SHM UART create / attach / I/O.
#[derive(Debug, thiserror::Error)]
pub enum ShmUartError {
    #[error("shared memory error: {0}")]
    Shmem(String),
    #[error("event error: {0}")]
    Event(String),
    #[error("invalid ring header (bad magic or capacity)")]
    BadHeader,
    #[error("{0}")]
    Message(String),
}

/// Creator-owned SHM segment (arbiter).
pub struct UartShmOwner {
    _shmem: Shmem,
    flink: PathBuf,
    capacity: u32,
}

impl UartShmOwner {
    /// Create a new flink-backed segment and initialize Event + ring.
    pub fn create(flink: impl Into<PathBuf>, capacity: u32) -> Result<Self, ShmUartError> {
        let capacity = capacity.max(2);
        let flink = flink.into();
        if let Some(parent) = flink.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ShmUartError::Message(e.to_string()))?;
        }
        if flink.exists() {
            let _ = std::fs::remove_file(&flink);
        }

        let size = estimate_size(capacity);
        let shmem = ShmemConf::new()
            .size(size)
            .flink(&flink)
            .create()
            .map_err(|e| ShmUartError::Shmem(e.to_string()))?;

        unsafe {
            let base = shmem.as_ptr();
            // TODO(ipc): Event::new is a raw_sync-specific create; isolate on swap.
            let (evt, evt_bytes) =
                Event::new(base, true).map_err(|e| ShmUartError::Event(e.to_string()))?;
            // Ensure clear initial state.
            let _ = evt.set(EventState::Clear);
            let header_ptr = aligned_header_ptr(base, evt_bytes);
            let header = &mut *header_ptr;
            header.magic = MAGIC;
            header.capacity = capacity;
            header.head = AtomicU32::new(0);
            header.tail = AtomicU32::new(0);
            header.drops = AtomicU64::new(0);
            let slots = slots_ptr(header_ptr, capacity);
            for i in 0..capacity as usize {
                *slots.add(i) = ShmUartFrame::default();
            }
        }

        Ok(Self {
            _shmem: shmem,
            flink,
            capacity,
        })
    }

    #[must_use]
    pub fn flink(&self) -> &Path {
        &self.flink
    }

    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.capacity
    }
}

impl Drop for UartShmOwner {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.flink);
    }
}

/// Attached producer or consumer endpoint.
pub struct UartShmEndpoint {
    _shmem: Shmem,
    evt_bytes: usize,
    capacity: u32,
}

impl UartShmEndpoint {
    /// Open an existing flink segment (node attach).
    pub fn open(flink: impl AsRef<Path>) -> Result<Self, ShmUartError> {
        let shmem = ShmemConf::new()
            .flink(flink.as_ref())
            .open()
            .map_err(|e| ShmUartError::Shmem(e.to_string()))?;
        let (capacity, evt_bytes) = unsafe {
            let base = shmem.as_ptr();
            let (_evt, evt_bytes) =
                Event::from_existing(base).map_err(|e| ShmUartError::Event(e.to_string()))?;
            let header = &*aligned_header_ptr(base, evt_bytes);
            if header.magic != MAGIC || header.capacity < 2 {
                return Err(ShmUartError::BadHeader);
            }
            (header.capacity, evt_bytes)
        };
        Ok(Self {
            _shmem: shmem,
            evt_bytes,
            capacity,
        })
    }

    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Push one frame. Returns `true` if an older frame was dropped.
    pub fn push(&self, frame: ShmUartFrame) -> Result<bool, ShmUartError> {
        unsafe {
            let base = self._shmem.as_ptr();
            let header = &mut *aligned_header_ptr(base, self.evt_bytes);
            let slots = slots_ptr(header as *mut RingHeader, self.capacity);
            let cap = self.capacity;
            let mut dropped = false;
            loop {
                let head = header.head.load(Ordering::Acquire);
                let tail = header.tail.load(Ordering::Acquire);
                let next = (head + 1) % cap;
                if next == tail {
                    // Drop oldest.
                    let new_tail = (tail + 1) % cap;
                    if header
                        .tail
                        .compare_exchange(tail, new_tail, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        continue;
                    }
                    header.drops.fetch_add(1, Ordering::Relaxed);
                    dropped = true;
                    continue;
                }
                *slots.add(head as usize) = frame;
                if header
                    .head
                    .compare_exchange(head, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    break;
                }
            }
            // TODO(ipc): wake path — replace raw_sync Event::set.
            let (evt, _) =
                Event::from_existing(base).map_err(|e| ShmUartError::Event(e.to_string()))?;
            evt.set(EventState::Signaled)
                .map_err(|e| ShmUartError::Event(e.to_string()))?;
            Ok(dropped)
        }
    }

    /// Pop one frame if available.
    pub fn try_pop(&self) -> Result<Option<ShmUartFrame>, ShmUartError> {
        unsafe {
            let base = self._shmem.as_ptr();
            let header = &*aligned_header_ptr(base, self.evt_bytes);
            let slots = slots_ptr(
                header as *const RingHeader as *mut RingHeader,
                self.capacity,
            );
            let cap = self.capacity;
            loop {
                let head = header.head.load(Ordering::Acquire);
                let tail = header.tail.load(Ordering::Acquire);
                if head == tail {
                    return Ok(None);
                }
                let frame = *slots.add(tail as usize);
                let next = (tail + 1) % cap;
                if header
                    .tail
                    .compare_exchange(tail, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Ok(Some(frame));
                }
            }
        }
    }

    /// Block until signaled (or timeout), then drain is left to the caller.
    pub fn wait(&self, timeout: Duration) -> Result<(), ShmUartError> {
        // TODO(ipc): wait path — replace raw_sync Event::wait.
        unsafe {
            let (evt, _) = Event::from_existing(self._shmem.as_ptr())
                .map_err(|e| ShmUartError::Event(e.to_string()))?;
            evt.wait(Timeout::Val(timeout))
                .map_err(|e| ShmUartError::Event(e.to_string()))?;
            Ok(())
        }
    }

    /// Cumulative overflow drops observed by producers.
    #[must_use]
    pub fn drops(&self) -> u64 {
        unsafe {
            let header = &*aligned_header_ptr(self._shmem.as_ptr(), self.evt_bytes);
            header.drops.load(Ordering::Relaxed)
        }
    }
}

fn estimate_size(capacity: u32) -> usize {
    // Event footprint is platform-dependent; reserve a generous prefix.
    512 + HEADER_ALIGN
        + std::mem::size_of::<RingHeader>()
        + capacity as usize * std::mem::size_of::<ShmUartFrame>()
        + 64
}

unsafe fn aligned_header_ptr(base: *mut u8, evt_bytes: usize) -> *mut RingHeader {
    // SAFETY: caller guarantees `base` points into a valid SHM mapping of sufficient size.
    let unaligned = unsafe { base.add(evt_bytes) };
    let align = HEADER_ALIGN;
    let addr = unaligned as usize;
    let aligned = (addr + align - 1) & !(align - 1);
    aligned as *mut RingHeader
}

unsafe fn slots_ptr(header: *mut RingHeader, _capacity: u32) -> *mut ShmUartFrame {
    // SAFETY: caller guarantees `header` is a valid RingHeader inside SHM.
    let after = unsafe { (header as *mut u8).add(std::mem::size_of::<RingHeader>()) };
    let align = std::mem::align_of::<ShmUartFrame>();
    let addr = after as usize;
    let aligned = (addr + align - 1) & !(align - 1);
    aligned as *mut ShmUartFrame
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn push_pop_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let flink = dir.path().join("uart.shm");
        let owner = UartShmOwner::create(&flink, 8).unwrap();
        let prod = UartShmEndpoint::open(owner.flink()).unwrap();
        let cons = UartShmEndpoint::open(owner.flink()).unwrap();

        let frame = ShmUartFrame {
            data: b'A' as u16,
            data_bits: 8,
            stop_bits: 1,
            parity: 0,
            inverted: 0,
            _pad: 0,
            bit_time_ns: 1000,
        };
        assert!(!prod.push(frame).unwrap());
        assert_eq!(cons.try_pop().unwrap(), Some(frame));
        assert_eq!(cons.try_pop().unwrap(), None);
    }

    #[test]
    fn overflow_drops_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let flink = dir.path().join("uart_ovf.shm");
        let owner = UartShmOwner::create(&flink, 4).unwrap();
        let prod = UartShmEndpoint::open(owner.flink()).unwrap();
        let cons = UartShmEndpoint::open(owner.flink()).unwrap();

        for i in 0..3 {
            assert!(
                !prod
                    .push(ShmUartFrame {
                        data: i,
                        ..ShmUartFrame::default()
                    })
                    .unwrap()
            );
        }
        // capacity 4 → usable 3; fourth drops oldest (0)
        assert!(
            prod.push(ShmUartFrame {
                data: 3,
                ..ShmUartFrame::default()
            })
            .unwrap()
        );
        assert!(prod.drops() >= 1);
        assert_eq!(cons.try_pop().unwrap().unwrap().data, 1);
    }

    #[test]
    fn event_wakes_waiter() {
        let dir = tempfile::tempdir().unwrap();
        let flink = dir.path().join("uart_wake.shm");
        let owner = UartShmOwner::create(&flink, 8).unwrap();
        let flink2 = owner.flink().to_path_buf();
        let cons = thread::spawn(move || {
            let ep = UartShmEndpoint::open(&flink2).unwrap();
            ep.wait(Duration::from_secs(2)).unwrap();
            ep.try_pop().unwrap()
        });
        thread::sleep(Duration::from_millis(50));
        let prod = UartShmEndpoint::open(owner.flink()).unwrap();
        let frame = ShmUartFrame {
            data: 7,
            ..ShmUartFrame::default()
        };
        prod.push(frame).unwrap();
        assert_eq!(cons.join().unwrap(), Some(frame));
    }
}
