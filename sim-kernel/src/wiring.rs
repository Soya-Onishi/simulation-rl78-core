//! Typed unidirectional wires and ports (no shared voltage net).
//!
//! Construction is typestate [`Wire<T, Sinks, Sources>`] (typenum counts):
//! either 1 source to N sinks or N sources to 1 sink. [`WiringBuilder::build`] freezes ready wires into [`SolidWire`]s
//! owned by [`crate::Machine`]. Peripherals keep [`SourcePort`] / sink callbacks
//! on `Arc` to the same solid wire and `drive` without going through [`crate::EventCtx`].

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use typenum::{Add1, B1, U2, U3, U4, U5, U6, U7, U8, Unsigned};

pub use typenum::{U0, U1};

thread_local! {
    static IN_DRIVE: Cell<bool> = const { Cell::new(false) };
}

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

/// Nested `drive` dropped because another callback is already on the stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NestedDrive;

/// Receives wire updates. Fan-out calls with `values.len() == 1` and `changed == 0`.
pub trait WireSink<T>: Send + Sync {
    fn on_input(&self, values: &[T], changed: usize);
}

struct Inner<T> {
    values: Vec<T>,
    sinks: Vec<Arc<dyn WireSink<T>>>,
    nested: Vec<NestedDrive>,
    frozen: bool,
}

impl<T> Inner<T> {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            sinks: Vec::new(),
            nested: Vec::new(),
            frozen: false,
        }
    }
}

/// Frozen wire after [`WiringBuilder::build`]. Ports hold clones of this `Arc`.
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

trait ErasedSolid: Send + Sync {
    fn take_nested(&self) -> Vec<NestedDrive>;
}

/// Type-erased solid wire for the [`Wiring`] bag.
pub struct WiringPiece {
    inner: Arc<dyn ErasedSolid>,
}

impl<T: Send + 'static> ErasedSolid for SolidWire<T> {
    fn take_nested(&self) -> Vec<NestedDrive> {
        std::mem::take(&mut self.inner.lock().expect("wire").nested)
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

impl<T: Clone + Send + 'static> SourcePort<T> {
    pub fn drive(&self, value: T) {
        let Some(solid) = self.wire.as_ref() else {
            return;
        };
        let nested = IN_DRIVE.with(|flag| {
            if flag.get() {
                true
            } else {
                flag.set(true);
                false
            }
        });
        if nested {
            solid.inner.lock().expect("wire").nested.push(NestedDrive);
            return;
        }
        struct DriveGuard;
        impl Drop for DriveGuard {
            fn drop(&mut self) {
                IN_DRIVE.with(|flag| flag.set(false));
            }
        }
        let _guard = DriveGuard;
        let (snapshot, sinks, changed) = {
            let mut g = solid.inner.lock().expect("wire");
            if self.index < g.values.len() {
                g.values[self.index] = value;
            }
            let changed = self.index;
            (g.values.clone(), g.sinks.clone(), changed)
        };
        for sink in sinks {
            sink.on_input(&snapshot, changed);
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
    debug_assert!(!g.frozen);
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

impl<T: Default + Clone + Send + 'static> Wire<T, U0, U0> {
    #[must_use]
    pub fn source(self, port: &mut SourcePort<T>) -> Wire<T, U0, U1> {
        attach_source(&self.inner, port);
        recast(self)
    }

    #[must_use]
    pub fn sink(self, sink: Arc<dyn WireSink<T>>) -> Wire<T, U1, U0> {
        self.inner.lock().expect("wire").sinks.push(sink);
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
    T: Default + Clone + Send + 'static,
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
    T: Clone + Send + 'static,
    Sinks: Unsigned + core::ops::Add<B1>,
    Add1<Sinks>: Unsigned,
{
    #[must_use]
    pub fn sink(self, sink: Arc<dyn WireSink<T>>) -> Wire<T, Add1<Sinks>, U1> {
        self.inner.lock().expect("wire").sinks.push(sink);
        recast(self)
    }
}

/// Wires that can enter the builder bag (1-1, 1-N, or N-1).
///
/// `freeze` is implemented for sink/source counts through [`typenum::U8`].
/// Attaching more ports still type-checks; add a `ReadyWire` impl if you need to `push` them.
pub trait ReadyWire: Sized {
    fn freeze(self) -> WiringPiece;
}

fn freeze_inner<T: Send + 'static>(inner: Arc<Mutex<Inner<T>>>) -> WiringPiece {
    inner.lock().expect("wire").frozen = true;
    WiringPiece {
        inner: Arc::new(SolidWire::from_inner(inner)),
    }
}

macro_rules! impl_ready_fan_out {
    ($($sinks:ty),*) => {
        $(impl<T: Send + 'static> ReadyWire for Wire<T, $sinks, U1> {
            fn freeze(self) -> WiringPiece {
                freeze_inner(self.inner)
            }
        })*
    };
}
impl_ready_fan_out!(U1, U2, U3, U4, U5, U6, U7, U8);

macro_rules! impl_ready_fan_in {
    ($($sources:ty),*) => {
        $(impl<T: Send + 'static> ReadyWire for Wire<T, U1, $sources> {
            fn freeze(self) -> WiringPiece {
                freeze_inner(self.inner)
            }
        })*
    };
}
impl_ready_fan_in!(U2, U3, U4, U5, U6, U7, U8);

/// Finished bag of solid wires owned by [`crate::Machine`].
#[derive(Default)]
pub struct Wiring {
    wires: Vec<Arc<dyn ErasedSolid>>,
}

impl Wiring {
    #[must_use]
    pub fn empty() -> Self {
        Self { wires: Vec::new() }
    }

    pub fn take_nested_drives(&mut self) -> Vec<NestedDrive> {
        let mut out = Vec::new();
        for w in &self.wires {
            out.extend(w.take_nested());
        }
        out
    }
}

/// Collects ready typestate wires, then [`Self::build`]s a [`Wiring`].
#[derive(Default)]
pub struct WiringBuilder {
    wires: Vec<Arc<dyn ErasedSolid>>,
}

impl WiringBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn push(mut self, wire: impl ReadyWire) -> Self {
        self.wires.push(wire.freeze().inner);
        self
    }

    #[must_use]
    pub fn build(self) -> Wiring {
        Wiring { wires: self.wires }
    }
}

/// Test source with a [`SourcePort`].
pub struct DummySource<T> {
    pub port: SourcePort<T>,
}

impl<T> DummySource<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            port: SourcePort::new(),
        }
    }
}

impl<T> Default for DummySource<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Test sink that records the last callback arguments.
pub struct DummySink<T> {
    last: Mutex<Option<(Vec<T>, usize)>>,
}

impl<T> DummySink<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            last: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn callback(self: &Arc<Self>) -> Arc<dyn WireSink<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        Arc::clone(self) as Arc<dyn WireSink<T>>
    }

    #[must_use]
    pub fn last(&self) -> Option<(Vec<T>, usize)>
    where
        T: Clone,
    {
        self.last.lock().expect("dummy sink").clone()
    }
}

impl<T> Default for DummySink<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + Send + Sync> WireSink<T> for DummySink<T> {
    fn on_input(&self, values: &[T], changed: usize) {
        *self.last.lock().expect("dummy sink") = Some((values.to_vec(), changed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_to_one_notifies_sink() {
        let mut src = DummySource::<u8>::new();
        let sink = Arc::new(DummySink::<u8>::new());
        let mut wiring = WiringBuilder::new()
            .push(Wire::new().source(&mut src.port).sink(sink.callback()))
            .build();
        src.port.drive(0x5A);
        assert_eq!(sink.last(), Some((vec![0x5A], 0)));
        assert!(wiring.take_nested_drives().is_empty());
    }

    #[test]
    fn fan_out_notifies_all_sinks() {
        let mut src = DummySource::<DigitalLevel>::new();
        let a = Arc::new(DummySink::<DigitalLevel>::new());
        let b = Arc::new(DummySink::<DigitalLevel>::new());
        let _wiring = WiringBuilder::new()
            .push(
                Wire::new()
                    .source(&mut src.port)
                    .sink(a.callback())
                    .sink(b.callback()),
            )
            .build();
        src.port.drive(DigitalLevel::High);
        assert_eq!(a.last(), Some((vec![DigitalLevel::High], 0)));
        assert_eq!(b.last(), Some((vec![DigitalLevel::High], 0)));
    }

    #[test]
    fn fan_in_reports_all_values_and_changed_index() {
        let mut s0 = DummySource::<u8>::new();
        let mut s1 = DummySource::<u8>::new();
        let sink = Arc::new(DummySink::<u8>::new());
        let _wiring = WiringBuilder::new()
            .push(
                Wire::new()
                    .sink(sink.callback())
                    .source(&mut s0.port)
                    .source(&mut s1.port),
            )
            .build();
        s0.port.drive(1);
        assert_eq!(sink.last(), Some((vec![1, 0], 0)));
        s1.port.drive(2);
        assert_eq!(sink.last(), Some((vec![1, 2], 1)));
    }

    #[test]
    fn nested_drive_same_callback_stack_logs() {
        let mut src = DummySource::<u8>::new();
        let mut chained = DummySource::<u8>::new();
        let chained_sink = Arc::new(DummySink::<u8>::new());

        struct Chain {
            next: Mutex<Option<SourcePort<u8>>>,
            seen: DummySink<u8>,
        }
        impl WireSink<u8> for Chain {
            fn on_input(&self, values: &[u8], changed: usize) {
                self.seen.on_input(values, changed);
                if let Some(v) = values.first().copied() {
                    if let Some(port) = self.next.lock().expect("next").as_ref() {
                        port.drive(v);
                    }
                }
            }
        }

        let chain = Arc::new(Chain {
            next: Mutex::new(None),
            seen: DummySink::new(),
        });
        let mut wiring = WiringBuilder::new()
            .push(
                Wire::new()
                    .source(&mut src.port)
                    .sink(Arc::clone(&chain) as _),
            )
            .push(
                Wire::new()
                    .source(&mut chained.port)
                    .sink(chained_sink.callback()),
            )
            .build();

        *chain.next.lock().expect("next") = Some(chained.port.clone());

        src.port.drive(7);
        assert_eq!(chain.seen.last(), Some((vec![7], 0)));
        assert!(chained_sink.last().is_none());
        assert_eq!(wiring.take_nested_drives().len(), 1);
    }

    #[test]
    fn analog_payload_is_independent_of_digital() {
        let mut src = DummySource::<AnalogVoltage>::new();
        let sink = Arc::new(DummySink::<AnalogVoltage>::new());
        let _wiring = WiringBuilder::new()
            .push(Wire::new().source(&mut src.port).sink(sink.callback()))
            .build();
        src.port.drive(AnalogVoltage(3_300_000));
        assert_eq!(sink.last(), Some((vec![AnalogVoltage(3_300_000)], 0)));
    }

    #[test]
    fn empty_wiring_builds() {
        let _ = WiringBuilder::new().build();
    }
}
