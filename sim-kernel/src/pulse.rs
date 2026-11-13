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
    /// Virtual time when the current hi/lo phase began.
    phase_started_at: Tick,
    event_id: Option<EventId>,
}

struct PulseExpanderHandle {
    ctl: EventCtl,
    output: SourcePort<DigitalLevel>,
    state: Mutex<PulseState>,
}

impl PulseExpanderHandle {
    fn is_running(st: &PulseState) -> bool {
        st.hi_ns > 0 && st.lo_ns > 0
    }

    fn cancel_event(&self, st: &mut PulseState) {
        if let Some(id) = st.event_id.take() {
            self.ctl.cancel(id);
        }
    }

    fn schedule_edge(self: &Arc<Self>, st: &mut PulseState, at: Tick) {
        let id = self.ctl.schedule(
            at,
            Box::new(PulseEdgeEvent {
                handle: Arc::clone(self),
            }),
        );
        st.event_id = Some(id);
    }

    fn drive_level(&self, st: &mut PulseState, high: bool) {
        if st.high == high {
            return;
        }
        st.high = high;
        self.output.drive(if high {
            DigitalLevel::High
        } else {
            DigitalLevel::Low
        });
    }

    fn stop_train(&self, st: &mut PulseState) {
        self.cancel_event(st);
        st.hi_ns = 0;
        st.lo_ns = 0;
        st.high = false;
        st.phase_started_at = self.ctl.now();
        self.output.drive(DigitalLevel::Low);
    }

    fn start_train(self: &Arc<Self>, st: &mut PulseState, pulse: Pulse, now: Tick) {
        self.cancel_event(st);
        st.hi_ns = pulse.hi_width_ns;
        st.lo_ns = pulse.lo_width_ns;
        st.high = true;
        st.phase_started_at = now;
        self.output.drive(DigitalLevel::High);
        self.schedule_edge(st, now.saturating_add(Tick(st.hi_ns)));
    }

    /// Retarget widths while running: keep phase elapsed, cascade overshoot.
    fn retarget_running(self: &Arc<Self>, st: &mut PulseState, pulse: Pulse, now: Tick) {
        self.cancel_event(st);
        let mut elapsed = now.0.saturating_sub(st.phase_started_at.0);
        let mut high = st.high;
        st.hi_ns = pulse.hi_width_ns;
        st.lo_ns = pulse.lo_width_ns;

        loop {
            let width = if high { st.hi_ns } else { st.lo_ns };
            if elapsed < width {
                let remainder = width - elapsed;
                self.drive_level(st, high);
                st.phase_started_at = Tick(now.0.saturating_sub(elapsed));
                self.schedule_edge(st, now.saturating_add(Tick(remainder)));
                return;
            }
            elapsed -= width;
            high = !high;
            self.drive_level(st, high);
        }
    }

    fn apply_pulse(self: &Arc<Self>, pulse: Pulse) {
        let now = self.ctl.now();
        let mut st = self.state.lock().expect("pulse state");

        if st.hi_ns == pulse.hi_width_ns && st.lo_ns == pulse.lo_width_ns {
            return;
        }

        if pulse.is_stopped() {
            self.stop_train(&mut st);
            return;
        }

        if !Self::is_running(&st) {
            self.start_train(&mut st, pulse, now);
            return;
        }

        self.retarget_running(&mut st, pulse, now);
    }

    fn on_edge(self: &Arc<Self>, now: Tick) {
        let mut st = self.state.lock().expect("pulse state");
        st.event_id = None;
        if !Self::is_running(&st) {
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
        st.phase_started_at = now;
        let width = if st.high { st.hi_ns } else { st.lo_ns };
        self.schedule_edge(&mut st, now.saturating_add(Tick(width)));
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
///
/// Re-driving an unchanged [`Pulse`] is ignored. A changed running pulse keeps
/// the current level's elapsed time and reschedules the next edge; overshoot
/// past the new phase width flips immediately and subtracts from subsequent
/// phases (cascading when needed).
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
                    phase_started_at: Tick(0),
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
    /// Combinational expander: do not clear the train here. Upstream sequential
    /// devices re-drive [`Self::pulse_sink`] after their own reset.
    fn reset(&mut self, _bus: &mut MemoryBus) {}
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

    fn next_deadline(ctl: &EventCtl) -> Option<Tick> {
        ctl.events().next_deadline()
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

    #[test]
    fn either_width_zero_stops_without_scheduling() {
        let ctl = EventCtl::new();
        ctl.set_now(Tick(0));
        let mut expander = PulseExpander::new("flow", ctl.clone()).expect("new");
        let levels = record_levels(expander.level_source_mut());
        let sink = expander.pulse_sink();
        sink(
            &[Pulse {
                hi_width_ns: 0,
                lo_width_ns: 1_000,
            }],
            0,
        );
        assert_eq!(*levels.lock().expect("levels"), vec![DigitalLevel::Low]);
        assert!(next_deadline(&ctl).is_none());
        fire_due(&ctl, Tick(1_000));
        assert_eq!(*levels.lock().expect("levels"), vec![DigitalLevel::Low]);
    }

    #[test]
    fn identical_pulse_does_not_restart_train() {
        let ctl = EventCtl::new();
        ctl.set_now(Tick(0));
        let mut expander = PulseExpander::new("flow", ctl.clone()).expect("new");
        let levels = record_levels(expander.level_source_mut());
        let sink = expander.pulse_sink();
        let pulse = Pulse {
            hi_width_ns: 1_000,
            lo_width_ns: 1_000,
        };
        sink(&[pulse], 0);
        ctl.set_now(Tick(400));
        sink(&[pulse], 0);
        assert_eq!(next_deadline(&ctl), Some(Tick(1_000)));
        fire_due(&ctl, Tick(1_000));
        assert_eq!(
            *levels.lock().expect("levels"),
            vec![DigitalLevel::High, DigitalLevel::Low]
        );
    }

    #[test]
    fn retarget_shortens_remaining_hi() {
        let ctl = EventCtl::new();
        ctl.set_now(Tick(0));
        let mut expander = PulseExpander::new("flow", ctl.clone()).expect("new");
        let levels = record_levels(expander.level_source_mut());
        let sink = expander.pulse_sink();
        sink(
            &[Pulse {
                hi_width_ns: 10_000_000,
                lo_width_ns: 20_000_000,
            }],
            0,
        );
        ctl.set_now(Tick(5_000_000));
        sink(
            &[Pulse {
                hi_width_ns: 8_000_000,
                lo_width_ns: 22_000_000,
            }],
            0,
        );
        assert_eq!(*levels.lock().expect("levels"), vec![DigitalLevel::High]);
        assert_eq!(next_deadline(&ctl), Some(Tick(8_000_000)));
        fire_due(&ctl, Tick(8_000_000));
        assert_eq!(
            *levels.lock().expect("levels"),
            vec![DigitalLevel::High, DigitalLevel::Low]
        );
        assert_eq!(next_deadline(&ctl), Some(Tick(30_000_000)));
    }

    #[test]
    fn retarget_overshoot_into_lo() {
        let ctl = EventCtl::new();
        ctl.set_now(Tick(0));
        let mut expander = PulseExpander::new("flow", ctl.clone()).expect("new");
        let levels = record_levels(expander.level_source_mut());
        let sink = expander.pulse_sink();
        sink(
            &[Pulse {
                hi_width_ns: 10_000_000,
                lo_width_ns: 20_000_000,
            }],
            0,
        );
        ctl.set_now(Tick(8_000_000));
        sink(
            &[Pulse {
                hi_width_ns: 5_000_000,
                lo_width_ns: 25_000_000,
            }],
            0,
        );
        assert_eq!(
            *levels.lock().expect("levels"),
            vec![DigitalLevel::High, DigitalLevel::Low]
        );
        // excess 3ms subtracted from new lo 25ms → next edge in 22ms
        assert_eq!(next_deadline(&ctl), Some(Tick(30_000_000)));
        fire_due(&ctl, Tick(30_000_000));
        assert_eq!(
            *levels.lock().expect("levels"),
            vec![DigitalLevel::High, DigitalLevel::Low, DigitalLevel::High]
        );
    }

    #[test]
    fn retarget_overshoot_cascades_across_phases() {
        let ctl = EventCtl::new();
        ctl.set_now(Tick(0));
        let mut expander = PulseExpander::new("flow", ctl.clone()).expect("new");
        let levels = record_levels(expander.level_source_mut());
        let sink = expander.pulse_sink();
        sink(
            &[Pulse {
                hi_width_ns: 10_000_000,
                lo_width_ns: 20_000_000,
            }],
            0,
        );
        ctl.set_now(Tick(8_000_000));
        // hi1/lo2 with 8ms into hi → cascade to lo with 1ms remaining
        sink(
            &[Pulse {
                hi_width_ns: 1_000_000,
                lo_width_ns: 2_000_000,
            }],
            0,
        );
        let got = levels.lock().expect("levels").clone();
        assert_eq!(got.last().copied(), Some(DigitalLevel::Low));
        assert_eq!(next_deadline(&ctl), Some(Tick(9_000_000)));
    }
}
