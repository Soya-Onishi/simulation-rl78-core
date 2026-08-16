//! Typed unidirectional wires and ports (no shared voltage net).
//!
//! Construction is typestate [`Wire<T, Sinks, Sources>`] (typenum counts):
//! either 1 source to N sinks or N sources to 1 sink. Ports keep `Arc`s to the
//! same inner wire; `drive` invokes sink closures immediately (no [`crate::EventCtx`]).

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use typenum::{Add1, B1, Unsigned};

use crate::Tick;

pub use typenum::{U0, U1};

/// Digital level used as a [`Wire`] payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DigitalLevel {
    #[default]
    HiZ,
    High,
    Low,
}

/// Analog voltage in microvolts, used as a [`Wire`] payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AnalogVoltage(pub i64);

/// UART stop-bit count carried on a [`UartFrame`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UartStopBits {
    #[default]
    One,
    Two,
}

/// UART parity carried on a [`UartFrame`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UartParity {
    #[default]
    None,
    Even,
    Odd,
}

/// One UART character plus link parameters, used as a [`Wire`] payload.
///
/// Serial lines are not modeled as [`DigitalLevel`] bit streams. `bit_time` is
/// the duration of one bit on the virtual clock (not a raw baud integer).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UartFrame {
    pub data: u16,
    pub data_bits: u8,
    pub bit_time: Tick,
    pub stop_bits: UartStopBits,
    pub parity: UartParity,
    pub inverted: bool,
}

impl Default for UartFrame {
    fn default() -> Self {
        Self {
            data: 0,
            data_bits: 8,
            bit_time: Tick::ZERO,
            stop_bits: UartStopBits::One,
            parity: UartParity::None,
            inverted: false,
        }
    }
}

type SinkFn<T> = Arc<dyn Fn(&[T], usize) + Send + Sync>;

struct Inner<T> {
    values: Vec<T>,
    sinks: Vec<SinkFn<T>>,
}

impl<T> Inner<T> {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            sinks: Vec::new(),
        }
    }
}

/// Shared wire state. [`SourcePort`] holds this `Arc` after [`Wire::source`].
pub struct SolidWire<T> {
    inner: Arc<Mutex<Inner<T>>>,
}

impl<T> Clone for SolidWire<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> SolidWire<T> {
    fn from_inner(inner: Arc<Mutex<Inner<T>>>) -> Self {
        Self { inner }
    }
}

/// Source endpoint owned by a peripheral. `drive` pushes into the bound wire.
pub struct SourcePort<T> {
    wire: Option<SolidWire<T>>,
    index: usize,
}

impl<T> Clone for SourcePort<T> {
    fn clone(&self) -> Self {
        Self {
            wire: self.wire.clone(),
            index: self.index,
        }
    }
}

impl<T> SourcePort<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            wire: None,
            index: 0,
        }
    }
}

impl<T> Default for SourcePort<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + 'static> SourcePort<T> {
    /// Updates this source and invokes sink closures immediately with `&values`.
    /// The inner lock is held for the callbacks so another `drive` on **this**
    /// wire deadlocks (`Mutex` is not reentrant). A sink may `drive` a **different**
    /// wire (combinational chain).
    pub fn drive(&self, value: T) {
        let Some(solid) = self.wire.as_ref() else {
            return;
        };
        let mut g = solid.inner.lock().expect("wire");
        if self.index < g.values.len() {
            g.values[self.index] = value;
        }
        let changed = self.index;
        let Inner { values, sinks } = &mut *g;
        let values = &*values;
        for sink in sinks.iter() {
            sink(values, changed);
        }
    }
}

fn bind_source<T>(port: &mut SourcePort<T>, inner: &Arc<Mutex<Inner<T>>>, index: usize) {
    port.wire = Some(SolidWire::from_inner(Arc::clone(inner)));
    port.index = index;
}

/// Building wire. `Sinks` / `Sources` are typenum unsigned counts (`U0`, `U1`, …).
///
/// Legal topologies: `(U0, U0)` while attaching, then 1→N (`Sources = U1`) or
/// N→1 (`Sinks = U1`). Both counts `> U1` has no `source`/`sink` methods.
/// Incomplete `(0, *)` / `(*, 0)` from generated wiring is allowed.
///
/// TODO: info-level log (do not reject) when a generated wire stays source-only,
/// sink-only, or empty (`Sinks::USIZE == 0` or `Sources::USIZE == 0`).
pub struct Wire<T, Sinks: Unsigned, Sources: Unsigned> {
    inner: Arc<Mutex<Inner<T>>>,
    _counts: PhantomData<(Sinks, Sources)>,
}

fn recast<T, Sinks, Sources, NewSinks, NewSources>(
    wire: Wire<T, Sinks, Sources>,
) -> Wire<T, NewSinks, NewSources>
where
    Sinks: Unsigned,
    Sources: Unsigned,
    NewSinks: Unsigned,
    NewSources: Unsigned,
{
    Wire {
        inner: wire.inner,
        _counts: PhantomData,
    }
}

fn attach_source<T: Default>(inner: &Arc<Mutex<Inner<T>>>, port: &mut SourcePort<T>) {
    let mut g = inner.lock().expect("wire");
    let index = g.values.len();
    g.values.push(T::default());
    bind_source(port, inner, index);
}

impl<T> Wire<T, U0, U0> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new())),
            _counts: PhantomData,
        }
    }
}

impl<T: Default + Send + 'static> Wire<T, U0, U0> {
    #[must_use]
    pub fn source(self, port: &mut SourcePort<T>) -> Wire<T, U0, U1> {
        attach_source(&self.inner, port);
        recast(self)
    }

    #[must_use]
    pub fn sink<F>(self, sink: F) -> Wire<T, U1, U0>
    where
        F: Fn(&[T], usize) + Send + Sync + 'static,
    {
        self.inner.lock().expect("wire").sinks.push(Arc::new(sink));
        recast(self)
    }
}

impl<T> Default for Wire<T, U0, U0> {
    fn default() -> Self {
        Self::new()
    }
}

/// Extra source: only while there is exactly one sink (N→1, including 0→1 → 1→1).
impl<T, Sources> Wire<T, U1, Sources>
where
    T: Default + Send + 'static,
    Sources: Unsigned + core::ops::Add<B1>,
    Add1<Sources>: Unsigned,
{
    #[must_use]
    pub fn source(self, port: &mut SourcePort<T>) -> Wire<T, U1, Add1<Sources>> {
        attach_source(&self.inner, port);
        recast(self)
    }
}

/// Extra sink: only while there is exactly one source (1→N, including 1→0 → 1→1).
impl<T, Sinks> Wire<T, Sinks, U1>
where
    T: Send + 'static,
    Sinks: Unsigned + core::ops::Add<B1>,
    Add1<Sinks>: Unsigned,
{
    #[must_use]
    pub fn sink<F>(self, sink: F) -> Wire<T, Add1<Sinks>, U1>
    where
        F: Fn(&[T], usize) + Send + Sync + 'static,
    {
        self.inner.lock().expect("wire").sinks.push(Arc::new(sink));
        recast(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummySource<T> {
        port: SourcePort<T>,
    }

    impl<T> DummySource<T> {
        fn new() -> Self {
            Self {
                port: SourcePort::new(),
            }
        }
    }

    struct DummySink<T> {
        last: Arc<Mutex<Option<(Vec<T>, usize)>>>,
    }

    impl<T> Clone for DummySink<T> {
        fn clone(&self) -> Self {
            Self {
                last: Arc::clone(&self.last),
            }
        }
    }

    impl<T: Clone + Send + Sync + 'static> DummySink<T> {
        fn new() -> Self {
            Self {
                last: Arc::new(Mutex::new(None)),
            }
        }

        fn last(&self) -> Option<(Vec<T>, usize)> {
            self.last.lock().expect("dummy sink").clone()
        }

        fn callback(&self) -> impl Fn(&[T], usize) + Send + Sync + 'static {
            let last = Arc::clone(&self.last);
            move |values: &[T], changed| {
                *last.lock().expect("dummy sink") = Some((values.to_vec(), changed));
            }
        }
    }

    #[test]
    fn one_to_one_notifies_sink() {
        let mut src = DummySource::<u8>::new();
        let sink = DummySink::<u8>::new();
        let _w = Wire::new().source(&mut src.port).sink(sink.callback());
        src.port.drive(0x5A);
        assert_eq!(sink.last(), Some((vec![0x5A], 0)));
    }

    #[test]
    fn fan_out_notifies_all_sinks() {
        let mut src = DummySource::<DigitalLevel>::new();
        let a = DummySink::<DigitalLevel>::new();
        let b = DummySink::<DigitalLevel>::new();
        let _w = Wire::new()
            .source(&mut src.port)
            .sink(a.callback())
            .sink(b.callback());
        src.port.drive(DigitalLevel::High);
        assert_eq!(a.last(), Some((vec![DigitalLevel::High], 0)));
        assert_eq!(b.last(), Some((vec![DigitalLevel::High], 0)));
    }

    #[test]
    fn fan_in_reports_all_values_and_changed_index() {
        let mut s0 = DummySource::<u8>::new();
        let mut s1 = DummySource::<u8>::new();
        let sink = DummySink::<u8>::new();
        let _w = Wire::new()
            .sink(sink.callback())
            .source(&mut s0.port)
            .source(&mut s1.port);
        s0.port.drive(1);
        assert_eq!(sink.last(), Some((vec![1, 0], 0)));
        s1.port.drive(2);
        assert_eq!(sink.last(), Some((vec![1, 2], 1)));
    }

    #[test]
    fn drive_in_callback_reaches_next_wire() {
        let mut src = DummySource::<u8>::new();
        let mut chained = DummySource::<u8>::new();
        let chained_sink = DummySink::<u8>::new();
        let seen = DummySink::<u8>::new();

        let next = Arc::new(Mutex::new(None::<SourcePort<u8>>));
        let next_bind = Arc::clone(&next);
        let seen_cb = seen.callback();
        let _w0 = Wire::new()
            .source(&mut src.port)
            .sink(move |values, changed| {
                seen_cb(values, changed);
                if let Some(v) = values.first().copied() {
                    if let Some(port) = next_bind.lock().expect("next").as_ref() {
                        port.drive(v);
                    }
                }
            });
        let _w1 = Wire::new()
            .source(&mut chained.port)
            .sink(chained_sink.callback());

        *next.lock().expect("next") = Some(chained.port.clone());

        src.port.drive(7);
        assert_eq!(seen.last(), Some((vec![7], 0)));
        assert_eq!(chained_sink.last(), Some((vec![7], 0)));
    }

    #[test]
    fn analog_payload_is_independent_of_digital() {
        let mut src = DummySource::<AnalogVoltage>::new();
        let sink = DummySink::<AnalogVoltage>::new();
        let _w = Wire::new().source(&mut src.port).sink(sink.callback());
        src.port.drive(AnalogVoltage(3_300_000));
        assert_eq!(sink.last(), Some((vec![AnalogVoltage(3_300_000)], 0)));
    }

    #[test]
    fn uart_frame_payload_reaches_sink() {
        let mut src = DummySource::<UartFrame>::new();
        let sink = DummySink::<UartFrame>::new();
        let _w = Wire::new().source(&mut src.port).sink(sink.callback());
        let frame = UartFrame {
            data: b'A' as u16,
            ..UartFrame::default()
        };
        src.port.drive(frame);
        assert_eq!(sink.last(), Some((vec![frame], 0)));
    }

    #[test]
    fn sink_closure_owns_component() {
        let mut src = DummySource::<u8>::new();
        let recv = Arc::new(Mutex::new(None::<(Vec<u8>, usize)>));
        let recv_cb = Arc::clone(&recv);
        let _w = Wire::new()
            .source(&mut src.port)
            .sink(move |values, changed| {
                *recv_cb.lock().expect("recv") = Some((values.to_vec(), changed));
            });
        src.port.drive(0x5A);
        assert_eq!(*recv.lock().expect("recv"), Some((vec![0x5A], 0)));
    }
}
