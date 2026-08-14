//! Virtual-time event queue.
//!
//! Same role as QEMU virtual timers (`timer_init_ns` and friends): schedule work
//! on the nanosecond virtual clock, then run the CPU only until the next
//! deadline. Intended users:
//!
//! - peripheral timers / compare-match / timer IRQs (M2+)
//! - watchdogs and other stop conditions on the same clock as the CPU
//! - host-scheduled work that must stay synchronized with virtual time
//!
//! Deadlines are [`Tick`] nanoseconds. CPU quanta are sized as
//! `min(max_quantum_ns, next_deadline - now)` and converted to an instruction
//! budget with [`crate::SimConfig::ns_per_instruction`].

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::bus::MemoryBus;
use crate::clock::Tick;
use crate::stop::StopReason;

/// Identifier of a scheduled event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventId(pub u64);

/// Context handed to an event when it fires on the simulation thread.
pub struct EventCtx<'a> {
    pub now: Tick,
    pub bus: &'a mut MemoryBus,
    /// Event may request a stop (for example a watchdog).
    pub stop: Option<StopReason>,
}

/// Work scheduled against the virtual clock.
pub trait SimEvent: Send {
    fn fire(&mut self, ctx: &mut EventCtx<'_>);
}

struct Scheduled {
    at: Tick,
    seq: u64,
    id: EventId,
}

impl PartialEq for Scheduled {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.seq == other.seq
    }
}

impl Eq for Scheduled {}

impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at
            .cmp(&other.at)
            .then_with(|| self.seq.cmp(&other.seq))
    }
}

/// Min-heap of virtual-time events.
#[derive(Default)]
pub struct EventQueue {
    next_id: u64,
    next_seq: u64,
    heap: BinaryHeap<Reverse<Scheduled>>,
    events: HashMap<EventId, Box<dyn SimEvent>>,
    cancelled: HashSet<EventId>,
}

impl EventQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn schedule(&mut self, at: Tick, event: Box<dyn SimEvent>) -> EventId {
        self.next_id += 1;
        self.next_seq += 1;
        let id = EventId(self.next_id);
        self.heap.push(Reverse(Scheduled {
            at,
            seq: self.next_seq,
            id,
        }));
        self.events.insert(id, event);
        id
    }

    pub fn cancel(&mut self, id: EventId) -> bool {
        if self.events.remove(&id).is_some() {
            self.cancelled.insert(id);
            true
        } else {
            false
        }
    }

    /// Next non-cancelled deadline, if any.
    ///
    /// The pending set is a min-heap: after dropping cancelled entries, the
    /// earliest deadline is the heap front (`O(1)` peek). Callers must not scan
    /// the whole queue on each poll.
    pub fn next_deadline(&mut self) -> Option<Tick> {
        self.drop_stale();
        self.heap.peek().map(|Reverse(item)| item.at)
    }

    /// Pop the next event that is due at or before `now`.
    pub fn pop_due(&mut self, now: Tick) -> Option<(EventId, Box<dyn SimEvent>)> {
        self.drop_stale();
        let next_at = self.heap.peek().map(|Reverse(item)| item.at)?;
        if next_at > now {
            return None;
        }
        let Reverse(item) = self.heap.pop()?;
        let event = self.events.remove(&item.id)?;
        Some((item.id, event))
    }

    fn drop_stale(&mut self) {
        while let Some(Reverse(item)) = self.heap.peek() {
            if self.cancelled.remove(&item.id) || !self.events.contains_key(&item.id) {
                self.heap.pop();
                continue;
            }
            break;
        }
    }
}

/// Handle cloned into peripherals so they can `timer_mod` at MMIO time.
///
/// Time lives here (and on [`crate::VirtualClock`] in [`crate::Machine`]), not
/// on [`crate::MemoryBus`]. Devices schedule/cancel directly; the kernel does
/// not harvest every mapped region each poll.
#[derive(Clone)]
pub struct EventCtl {
    now: Arc<AtomicU64>,
    events: Arc<Mutex<EventQueue>>,
}

impl Default for EventCtl {
    fn default() -> Self {
        Self::new()
    }
}

impl EventCtl {
    #[must_use]
    pub fn new() -> Self {
        Self {
            now: Arc::new(AtomicU64::new(0)),
            events: Arc::new(Mutex::new(EventQueue::new())),
        }
    }

    #[must_use]
    pub fn now(&self) -> Tick {
        Tick(self.now.load(Ordering::SeqCst))
    }

    pub fn set_now(&self, now: Tick) {
        self.now.store(now.0, Ordering::SeqCst);
    }

    pub fn schedule(&self, at: Tick, event: Box<dyn SimEvent>) -> EventId {
        self.events.lock().expect("event queue").schedule(at, event)
    }

    pub fn cancel(&self, id: EventId) -> bool {
        self.events.lock().expect("event queue").cancel(id)
    }

    pub fn events(&self) -> MutexGuard<'_, EventQueue> {
        self.events.lock().expect("event queue")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::MemoryBus;

    struct Record(Tick);

    impl SimEvent for Record {
        fn fire(&mut self, ctx: &mut EventCtx<'_>) {
            self.0 = ctx.now;
        }
    }

    #[test]
    fn fires_in_time_order() {
        let mut q = EventQueue::new();
        q.schedule(Tick(10), Box::new(Record(Tick::ZERO)));
        q.schedule(Tick(5), Box::new(Record(Tick::ZERO)));
        assert_eq!(q.next_deadline(), Some(Tick(5)));

        let mut bus = MemoryBus::new();
        let (earlier_id, mut earlier) = q.pop_due(Tick(5)).unwrap();
        let mut ctx = EventCtx {
            now: Tick(5),
            bus: &mut bus,
            stop: None,
        };
        earlier.fire(&mut ctx);
        assert!(earlier_id.0 != 0);
        assert!(q.pop_due(Tick(5)).is_none());
        assert!(q.pop_due(Tick(10)).is_some());
    }

    #[test]
    fn cancel_skips_event() {
        let mut q = EventQueue::new();
        let id = q.schedule(Tick(1), Box::new(Record(Tick::ZERO)));
        assert!(q.cancel(id));
        assert_eq!(q.next_deadline(), None);
        assert!(q.pop_due(Tick(1)).is_none());
    }
}
