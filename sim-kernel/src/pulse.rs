//! Expand a [`Pulse`] board-edge payload into a GPIO [`DigitalLevel`] train.

use std::sync::{Arc, Mutex};

use crate::action::{Device, DeviceError, id_hash};
use crate::bus::MemoryBus;
use crate::clock::Tick;
use crate::event::{EventCtl, EventCtx, EventId, SimEvent};
use crate::reset::Resettable;
use crate::wiring::{DigitalLevel, Pulse, SourcePort};

struct PulseState {
    hi_ns: u64,
    lo_ns: u64,
    high: bool,
    event_id: Option<EventId>,
}

struct PulseExpanderHandle {
    ctl: EventCtl,
    output: SourcePort<DigitalLevel>,
    state: Mutex<PulseState>,
}

impl PulseExpanderHandle {
    fn apply_pulse(self: &Arc<Self>, pulse: Pulse) {
        let now = self.ctl.now();
        let mut st = self.state.lock().expect("pulse state");
        if let Some(id) = st.event_id.take() {
            self.ctl.cancel(id);
        }
        if pulse.is_stopped() {
            st.hi_ns = 0;
            st.lo_ns = 0;
            st.high = false;
            self.output.drive(DigitalLevel::Low);
            return;
        }
        st.hi_ns = pulse.hi_width_ns;
        st.lo_ns = pulse.lo_width_ns;
        st.high = true;
        self.output.drive(DigitalLevel::High);
        let at = now.saturating_add(Tick(st.hi_ns));
        let id = self.ctl.schedule(
            at,
            Box::new(PulseEdgeEvent {
                handle: Arc::clone(self),
            }),
        );
        st.event_id = Some(id);
    }

    fn on_edge(self: &Arc<Self>, now: Tick) {
        let mut st = self.state.lock().expect("pulse state");
        st.event_id = None;
        if st.hi_ns == 0 || st.lo_ns == 0 {
            st.high = false;
            self.output.drive(DigitalLevel::Low);
            return;
        }
        st.high = !st.high;
        let level = if st.high {
            DigitalLevel::High
        } else {
            DigitalLevel::Low
        };
        self.output.drive(level);
        let width = if st.high { st.hi_ns } else { st.lo_ns };
        let at = now.saturating_add(Tick(width));
        let id = self.ctl.schedule(
            at,
            Box::new(PulseEdgeEvent {
                handle: Arc::clone(self),
            }),
        );
        st.event_id = Some(id);
    }
}

struct PulseEdgeEvent {
    handle: Arc<PulseExpanderHandle>,
}

impl SimEvent for PulseEdgeEvent {
    fn fire(&mut self, ctx: &mut EventCtx<'_>) {
        self.handle.on_edge(ctx.now);
    }
}

/// Board device that expands [`Pulse`] settings into a GPIO level train.
///
/// Construction order:
/// 1. [`Self::new`]
/// 2. Wire [`Self::level_source_mut`] to the GPIO sink (while this is the only
///    `Arc` holder)
/// 3. Attach [`Self::pulse_sink`] to the board `InPort<Pulse>` wire
pub struct PulseExpander {
    name: String,
    name_hash: u64,
    handle: Arc<PulseExpanderHandle>,
}

impl PulseExpander {
    /// Create an expander named `name` that schedules edges on `ctl`.
    ///
    /// # Errors
    ///
    /// `name` is empty.
    pub fn new(name: impl Into<String>, ctl: EventCtl) -> Result<Self, DeviceError> {
        let name = name.into();
        if name.is_empty() {
            return Err(DeviceError::EmptyName);
        }
        Ok(Self {
            name_hash: id_hash(&name),
            name,
            handle: Arc::new(PulseExpanderHandle {
                ctl,
                output: SourcePort::new(),
                state: Mutex::new(PulseState {
                    hi_ns: 0,
                    lo_ns: 0,
                    high: false,
                    event_id: None,
                }),
            }),
        })
    }

    /// GPIO output driven by the expanded train.
    ///
    /// Must be wired before [`Self::pulse_sink`] clones the handle `Arc`.
    #[must_use]
    pub fn level_source_mut(&mut self) -> &mut SourcePort<DigitalLevel> {
        &mut Arc::get_mut(&mut self.handle)
            .expect("wire level_source_mut before pulse_sink")
            .output
    }

    /// Sink for an `InPort<Pulse>` / data-plane RX wire.
    #[must_use]
    pub fn pulse_sink(&self) -> impl Fn(&[Pulse], usize) + Send + Sync + 'static {
        let handle = Arc::clone(&self.handle);
        move |values: &[Pulse], changed: usize| {
            if let Some(pulse) = values.get(changed).copied() {
                handle.apply_pulse(pulse);
            }
        }
    }
}

impl Resettable for PulseExpander {
    fn reset(&mut self, _bus: &mut MemoryBus) {
        self.handle.apply_pulse(Pulse::STOPPED);
    }
}

impl Device for PulseExpander {
    fn name(&self) -> &str {
        &self.name
    }

    fn name_hash(&self) -> u64 {
        self.name_hash
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::wiring::Wire;

    fn record_levels(source: &mut SourcePort<DigitalLevel>) -> Arc<Mutex<Vec<DigitalLevel>>> {
        let levels = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&levels);
        let _wire = Wire::new().source(source).sink(move |values, changed| {
            if let Some(level) = values.get(changed).copied() {
                recorded.lock().expect("levels").push(level);
            }
        });
        let _ = _wire;
        levels
    }

    fn fire_due(ctl: &EventCtl, now: Tick) {
        ctl.set_now(now);
        let mut bus = MemoryBus::new();
        loop {
            let mut q = ctl.events();
            let Some((_id, mut ev)) = q.pop_due(now) else {
                break;
            };
            drop(q);
            let mut ctx = EventCtx {
                now,
                bus: &mut bus,
                stop: None,
            };
            ev.fire(&mut ctx);
        }
    }

    #[test]
    fn expanding_pulse_toggles_high_then_low() {
        let ctl = EventCtl::new();
        ctl.set_now(Tick(0));
        let mut expander = PulseExpander::new("flow", ctl.clone()).expect("new");
        let levels = record_levels(expander.level_source_mut());
        let sink = expander.pulse_sink();
        sink(
            &[Pulse {
                hi_width_ns: 1_000,
                lo_width_ns: 1_000,
            }],
            0,
        );
        assert_eq!(*levels.lock().expect("levels"), vec![DigitalLevel::High]);
        fire_due(&ctl, Tick(1_000));
        assert_eq!(
            *levels.lock().expect("levels"),
            vec![DigitalLevel::High, DigitalLevel::Low]
        );
        fire_due(&ctl, Tick(2_000));
        assert_eq!(
            *levels.lock().expect("levels"),
            vec![DigitalLevel::High, DigitalLevel::Low, DigitalLevel::High]
        );
    }

    #[test]
    fn stopped_pulse_holds_low() {
        let ctl = EventCtl::new();
        ctl.set_now(Tick(0));
        let mut expander = PulseExpander::new("flow", ctl.clone()).expect("new");
        let levels = record_levels(expander.level_source_mut());
        let sink = expander.pulse_sink();
        sink(
            &[Pulse {
                hi_width_ns: 500,
                lo_width_ns: 500,
            }],
            0,
        );
        sink(&[Pulse::STOPPED], 0);
        fire_due(&ctl, Tick(500));
        let got = levels.lock().expect("levels").clone();
        assert_eq!(got, vec![DigitalLevel::High, DigitalLevel::Low]);
    }
}
