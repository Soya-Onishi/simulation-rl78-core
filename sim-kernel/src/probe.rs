//! Wire-side probe taps.
//!
//! A [`ProbeTap`] is attached with [`crate::Wire::sink`] like any other peripheral.
//! It applies a [`ProbePolicy`] and keeps the latest accepted sample. Cluster IPC
//! publishers are a separate object, built later from the [`ProbePoint`] values
//! returned by the board builder.

use std::sync::{Arc, Mutex};

use crate::virtual_time::virtual_time_ns;
use crate::wiring::{AnalogVoltage, DigitalLevel, UartFrame};

/// When a [`ProbeTap`] accepts a driven value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbePolicy {
    /// Accept when the value differs from the last accepted value.
    OnChange,
    /// Accept every `drive` that reaches this sink.
    EveryDrive,
    /// Accept at most once per `min_ns` of virtual time (`0` = every drive).
    MinInterval { min_ns: u64 },
}

/// One accepted sample waiting for the cluster publisher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeSample<T> {
    pub virtual_time_ns: u64,
    pub value: T,
}

struct ProbeState<T> {
    policy: ProbePolicy,
    last_value: Option<T>,
    last_sent_vt: Option<u64>,
    pending: Option<ProbeSample<T>>,
}

impl<T: Copy + PartialEq> ProbeState<T> {
    fn on_drive(&mut self, value: T, vt: u64) {
        let accept = match self.policy {
            ProbePolicy::OnChange => self.last_value != Some(value),
            ProbePolicy::EveryDrive => true,
            ProbePolicy::MinInterval { min_ns } => match self.last_sent_vt {
                None => true,
                Some(prev) => vt.saturating_sub(prev) >= min_ns,
            },
        };
        if !accept {
            return;
        }
        self.last_value = Some(value);
        self.last_sent_vt = Some(vt);
        self.pending = Some(ProbeSample {
            virtual_time_ns: vt,
            value,
        });
    }
}

/// One probe point attached as a [`crate::Wire`] sink.
///
/// [`ProbeTap::new`] does not open IPC. The board builder returns the [`Arc`]
/// (usually wrapped in [`ProbePoint`]); the cluster runtime creates the publisher.
pub struct ProbeTap<T> {
    name: String,
    state: Mutex<ProbeState<T>>,
}

impl<T> ProbeTap<T>
where
    T: Copy + PartialEq + Send + 'static,
{
    /// Create a wiring-side tap. The name is unique on the board.
    #[must_use]
    pub fn new(name: impl Into<String>, policy: ProbePolicy) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            state: Mutex::new(ProbeState {
                policy,
                last_value: None,
                last_sent_vt: None,
                pending: None,
            }),
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Closure suitable for [`crate::Wire::sink`].
    #[must_use]
    pub fn callback(self: &Arc<Self>) -> impl Fn(&[T], usize) + Send + Sync + 'static {
        let tap = Arc::clone(self);
        move |values, changed| tap.on_input(values, changed)
    }

    /// Take the latest accepted sample, if the publisher has not collected it yet.
    #[must_use]
    pub fn take_pending(&self) -> Option<ProbeSample<T>> {
        self.state.lock().expect("probe state").pending.take()
    }

    fn on_input(&self, values: &[T], changed: usize) {
        let Some(value) = values.get(changed).copied() else {
            return;
        };
        self.state
            .lock()
            .expect("probe state")
            .on_drive(value, virtual_time_ns());
    }
}

/// Heterogeneous list of taps returned by a board builder.
#[derive(Clone)]
pub enum ProbePoint {
    Digital(Arc<ProbeTap<DigitalLevel>>),
    Uart(Arc<ProbeTap<UartFrame>>),
    Analog(Arc<ProbeTap<AnalogVoltage>>),
}

impl ProbePoint {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Digital(tap) => tap.name(),
            Self::Uart(tap) => tap.name(),
            Self::Analog(tap) => tap.name(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set_global_virtual_time_ns;

    #[test]
    fn on_change_skips_duplicates() {
        let tap = ProbeTap::new("vcc", ProbePolicy::OnChange);
        let cb = tap.callback();
        cb(&[DigitalLevel::High], 0);
        assert_eq!(
            tap.take_pending().map(|s| s.value),
            Some(DigitalLevel::High)
        );
        cb(&[DigitalLevel::High], 0);
        assert!(tap.take_pending().is_none());
        cb(&[DigitalLevel::Low], 0);
        assert_eq!(tap.take_pending().map(|s| s.value), Some(DigitalLevel::Low));
    }

    #[test]
    fn every_drive_keeps_latest() {
        let tap = ProbeTap::new("vcc", ProbePolicy::EveryDrive);
        let cb = tap.callback();
        cb(&[DigitalLevel::High], 0);
        cb(&[DigitalLevel::Low], 0);
        assert_eq!(tap.take_pending().map(|s| s.value), Some(DigitalLevel::Low));
    }

    #[test]
    fn min_interval_throttles() {
        set_global_virtual_time_ns(0);
        let tap = ProbeTap::new("vcc", ProbePolicy::MinInterval { min_ns: 100 });
        let cb = tap.callback();
        cb(&[DigitalLevel::High], 0);
        assert!(tap.take_pending().is_some());
        set_global_virtual_time_ns(50);
        cb(&[DigitalLevel::Low], 0);
        assert!(tap.take_pending().is_none());
        set_global_virtual_time_ns(100);
        cb(&[DigitalLevel::Low], 0);
        assert!(tap.take_pending().is_some());
        set_global_virtual_time_ns(0);
    }
}
