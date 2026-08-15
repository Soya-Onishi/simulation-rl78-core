//! Typed unidirectional wires and ports (no shared voltage net).
//!
//! Construction is typestate [`Wire<T, C, Sinks, Sources>`] (typenum counts):
//! either 1 source to N sinks or N sources to 1 sink. Ports keep `Arc`s to the
//! same inner wire; `drive` invokes sinks with `&mut C` and no [`crate::EventCtx`].

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use typenum::{Add1, B1, Unsigned};

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

/// Receives wire updates. `C` is the sink-side component (`drive` passes `&mut C`).
/// Fan-out calls with `values.len() == 1` and `changed == 0`.
pub trait WireSink<T, C: ?Sized>: Send + Sync {
    fn on_input(&self, component: &mut C, values: &[T], changed: usize);
}

struct Inner<T, C: ?Sized> {
    values: Vec<T>,
    sinks: Vec<Arc<dyn WireSink<T, C>>>,
}

impl<T, C: ?Sized> Inner<T, C> {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            sinks: Vec::new(),
        }
    }
}

/// Shared wire state. [`SourcePort`] holds this `Arc` after [`Wire::source`].
pub struct SolidWire<T, C: ?Sized> {
    inner: Arc<Mutex<Inner<T, C>>>,
}

impl<T, C: ?Sized> Clone for SolidWire<T, C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T, C: ?Sized> SolidWire<T, C> {
    fn from_inner(inner: Arc<Mutex<Inner<T, C>>>) -> Self {
        Self { inner }
    }
}

/// Source endpoint owned by a peripheral. `drive` pushes into the bound wire.
pub struct SourcePort<T, C: ?Sized = ()> {
    wire: Option<SolidWire<T, C>>,
    index: usize,
    _c: PhantomData<fn(&mut C)>,
}

impl<T, C: ?Sized> Clone for SourcePort<T, C> {
    fn clone(&self) -> Self {
        Self {
            wire: self.wire.clone(),
            index: self.index,
            _c: PhantomData,
        }
    }
}

impl<T, C: ?Sized> SourcePort<T, C> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            wire: None,
            index: 0,
            _c: PhantomData,
        }
    }
}

impl<T, C: ?Sized> Default for SourcePort<T, C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + 'static, C: ?Sized + 'static> SourcePort<T, C> {
    /// Updates this source and invokes sinks immediately with `&mut component`.
    /// The inner lock is held for the callbacks so another `drive` on **this**
    /// wire deadlocks (`Mutex` is not reentrant). A sink may `drive` a **different**
    /// wire (combinational chain).
    pub fn drive(&self, value: T, component: &mut C) {
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
            sink.on_input(component, values, changed);
        }
    }
}

fn bind_source<T, C: ?Sized>(
    port: &mut SourcePort<T, C>,
    inner: &Arc<Mutex<Inner<T, C>>>,
    index: usize,
) {
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
pub struct Wire<T, C: ?Sized, Sinks: Unsigned, Sources: Unsigned> {
    inner: Arc<Mutex<Inner<T, C>>>,
    _counts: PhantomData<(Sinks, Sources)>,
}

fn recast<T, C, Sinks, Sources, NewSinks, NewSources>(
    wire: Wire<T, C, Sinks, Sources>,
) -> Wire<T, C, NewSinks, NewSources>
where
    C: ?Sized,
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

fn attach_source<T: Default, C: ?Sized>(
    inner: &Arc<Mutex<Inner<T, C>>>,
    port: &mut SourcePort<T, C>,
) {
    let mut g = inner.lock().expect("wire");
    let index = g.values.len();
    g.values.push(T::default());
    bind_source(port, inner, index);
}

impl<T, C: ?Sized> Wire<T, C, U0, U0> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new())),
            _counts: PhantomData,
        }
    }
}

impl<T, C> Wire<T, C, U0, U0>
where
    T: Default + Send + 'static,
    C: ?Sized + 'static,
{
    #[must_use]
    pub fn source(self, port: &mut SourcePort<T, C>) -> Wire<T, C, U0, U1> {
        attach_source(&self.inner, port);
        recast(self)
    }

    #[must_use]
    pub fn sink<K>(self, sink: K) -> Wire<T, C, U1, U0>
    where
        K: WireSink<T, C> + 'static,
    {
        self.inner.lock().expect("wire").sinks.push(Arc::new(sink));
        recast(self)
    }
}

impl<T, C: ?Sized> Default for Wire<T, C, U0, U0> {
    fn default() -> Self {
        Self::new()
    }
}

/// Extra source: only while there is exactly one sink (N→1, including 0→1 → 1→1).
impl<T, C, Sources> Wire<T, C, U1, Sources>
where
    T: Default + Send + 'static,
    C: ?Sized + 'static,
    Sources: Unsigned + core::ops::Add<B1>,
    Add1<Sources>: Unsigned,
{
    #[must_use]
    pub fn source(self, port: &mut SourcePort<T, C>) -> Wire<T, C, U1, Add1<Sources>> {
        attach_source(&self.inner, port);
        recast(self)
    }
}

/// Extra sink: only while there is exactly one source (1→N, including 1→0 → 1→1).
impl<T, C, Sinks> Wire<T, C, Sinks, U1>
where
    T: Send + 'static,
    C: ?Sized + 'static,
    Sinks: Unsigned + core::ops::Add<B1>,
    Add1<Sinks>: Unsigned,
{
    #[must_use]
    pub fn sink<K>(self, sink: K) -> Wire<T, C, Add1<Sinks>, U1>
    where
        K: WireSink<T, C> + 'static,
    {
        self.inner.lock().expect("wire").sinks.push(Arc::new(sink));
        recast(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummySource<T, C: ?Sized = ()> {
        port: SourcePort<T, C>,
    }

    impl<T, C: ?Sized> DummySource<T, C> {
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

    impl<T> DummySink<T> {
        fn new() -> Self {
            Self {
                last: Arc::new(Mutex::new(None)),
            }
        }

        fn last(&self) -> Option<(Vec<T>, usize)>
        where
            T: Clone,
        {
            self.last.lock().expect("dummy sink").clone()
        }
    }

    impl<T: Clone + Send + Sync> WireSink<T, ()> for DummySink<T> {
        fn on_input(&self, _component: &mut (), values: &[T], changed: usize) {
            *self.last.lock().expect("dummy sink") = Some((values.to_vec(), changed));
        }
    }

    #[test]
    fn one_to_one_notifies_sink() {
        let mut src = DummySource::<u8>::new();
        let sink = DummySink::<u8>::new();
        let _w = Wire::new().source(&mut src.port).sink(sink.clone());
        src.port.drive(0x5A, &mut ());
        assert_eq!(sink.last(), Some((vec![0x5A], 0)));
    }

    #[test]
    fn fan_out_notifies_all_sinks() {
        let mut src = DummySource::<DigitalLevel>::new();
        let a = DummySink::<DigitalLevel>::new();
        let b = DummySink::<DigitalLevel>::new();
        let _w = Wire::new()
            .source(&mut src.port)
            .sink(a.clone())
            .sink(b.clone());
        src.port.drive(DigitalLevel::High, &mut ());
        assert_eq!(a.last(), Some((vec![DigitalLevel::High], 0)));
        assert_eq!(b.last(), Some((vec![DigitalLevel::High], 0)));
    }

    #[test]
    fn fan_in_reports_all_values_and_changed_index() {
        let mut s0 = DummySource::<u8>::new();
        let mut s1 = DummySource::<u8>::new();
        let sink = DummySink::<u8>::new();
        let _w = Wire::new()
            .sink(sink.clone())
            .source(&mut s0.port)
            .source(&mut s1.port);
        s0.port.drive(1, &mut ());
        assert_eq!(sink.last(), Some((vec![1, 0], 0)));
        s1.port.drive(2, &mut ());
        assert_eq!(sink.last(), Some((vec![1, 2], 1)));
    }

    #[test]
    fn on_input_mutates_sink_component() {
        struct Recv {
            last: Option<(Vec<u8>, usize)>,
        }
        struct RecvPin;
        impl WireSink<u8, Recv> for RecvPin {
            fn on_input(&self, recv: &mut Recv, values: &[u8], changed: usize) {
                recv.last = Some((values.to_vec(), changed));
            }
        }

        let mut src = DummySource::<u8, Recv>::new();
        let mut recv = Recv { last: None };
        let _w = Wire::new().source(&mut src.port).sink(RecvPin);
        src.port.drive(0x5A, &mut recv);
        assert_eq!(recv.last, Some((vec![0x5A], 0)));
    }

    #[test]
    fn drive_in_callback_reaches_next_wire() {
        let mut src = DummySource::<u8>::new();
        let mut chained = DummySource::<u8>::new();
        let chained_sink = DummySink::<u8>::new();

        struct Chain {
            next: Mutex<Option<SourcePort<u8>>>,
            seen: DummySink<u8>,
        }
        impl WireSink<u8, ()> for Arc<Chain> {
            fn on_input(&self, component: &mut (), values: &[u8], changed: usize) {
                self.seen.on_input(component, values, changed);
                if let Some(v) = values.first().copied() {
                    if let Some(port) = self.next.lock().expect("next").as_ref() {
                        port.drive(v, component);
                    }
                }
            }
        }

        let chain = Arc::new(Chain {
            next: Mutex::new(None),
            seen: DummySink::new(),
        });
        let _w0 = Wire::new().source(&mut src.port).sink(Arc::clone(&chain));
        let _w1 = Wire::new()
            .source(&mut chained.port)
            .sink(chained_sink.clone());

        *chain.next.lock().expect("next") = Some(chained.port.clone());

        src.port.drive(7, &mut ());
        assert_eq!(chain.seen.last(), Some((vec![7], 0)));
        assert_eq!(chained_sink.last(), Some((vec![7], 0)));
    }

    #[test]
    fn analog_payload_is_independent_of_digital() {
        let mut src = DummySource::<AnalogVoltage>::new();
        let sink = DummySink::<AnalogVoltage>::new();
        let _w = Wire::new().source(&mut src.port).sink(sink.clone());
        src.port.drive(AnalogVoltage(3_300_000), &mut ());
        assert_eq!(sink.last(), Some((vec![AnalogVoltage(3_300_000)], 0)));
    }
}
