//! Pin / net interconnect built in Rust code (no config files).
//!
//! Same construction style as [`crate::MemoryMapBuilder`]: register endpoints,
//! [`WiringBuilder::connect`] / [`WiringBuilder::pull`], then [`WiringBuilder::build`].
//! After `build`, the map is immutable — live reconnect is not supported.
//!
//! Wires are unidirectional. A bidirectional hardware pin exposes both a source
//! and a sink (or a [`PinRole::Bidirectional`] digital pin). Typed ports cover
//! QEMU-style gpio/irq links whose payload is not limited to a level.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_PIN: AtomicU32 = AtomicU32::new(1);
static NEXT_PORT: AtomicU32 = AtomicU32::new(1);

fn alloc_pin() -> PinId {
    PinId(NEXT_PIN.fetch_add(1, Ordering::Relaxed))
}

fn alloc_port() -> PortId {
    PortId(NEXT_PORT.fetch_add(1, Ordering::Relaxed))
}

/// Stable handle for a digital or analog endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PinId(pub u32);

/// Stable handle for a typed [`SourcePort`] / [`SinkPort`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PortId(pub u32);

/// Digital drive value. Combinational nets resolve `HiZ` via pull if present.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DigitalLevel {
    High,
    Low,
    HiZ,
}

/// Analog voltage in microvolts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AnalogVoltage(pub i64);

/// Digital vs analog net. Mixing kinds is a build error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignalKind {
    Digital,
    Analog,
}

/// Who may drive a pin. Wires run source → sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PinRole {
    Source,
    Sink,
    Bidirectional,
}

/// Endpoint handle returned by dummy devices and (later) GPIO.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PinRef {
    id: PinId,
    kind: SignalKind,
    role: PinRole,
}

impl PinRef {
    #[must_use]
    pub fn id(self) -> PinId {
        self.id
    }

    #[must_use]
    pub fn kind(self) -> SignalKind {
        self.kind
    }

    #[must_use]
    pub fn role(self) -> PinRole {
        self.role
    }

    #[must_use]
    fn can_drive(self) -> bool {
        matches!(self.role, PinRole::Source | PinRole::Bidirectional)
    }

    #[must_use]
    fn can_sense(self) -> bool {
        matches!(self.role, PinRole::Sink | PinRole::Bidirectional)
    }
}

/// Component endpoint that participates in wiring.
pub trait Endpoint {
    fn pin(&self) -> PinRef;
}

/// Typed unidirectional source (QEMU `qemu_irq` / `gpio_out` analogue).
#[derive(Debug)]
pub struct SourcePort<T> {
    id: PortId,
    _ty: PhantomData<fn() -> T>,
}

impl<T> Clone for SourcePort<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for SourcePort<T> {}

impl<T> PartialEq for SourcePort<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for SourcePort<T> {}

impl<T> SourcePort<T> {
    #[must_use]
    pub fn id(self) -> PortId {
        self.id
    }
}

/// Typed unidirectional sink (QEMU `gpio_in` analogue).
#[derive(Debug)]
pub struct SinkPort<T> {
    id: PortId,
    _ty: PhantomData<fn() -> T>,
}

impl<T> Clone for SinkPort<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for SinkPort<T> {}

impl<T> PartialEq for SinkPort<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for SinkPort<T> {}

impl<T> SinkPort<T> {
    #[must_use]
    pub fn id(self) -> PortId {
        self.id
    }
}

/// Net pull when every digital driver is [`DigitalLevel::HiZ`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pull {
    Up,
    Down,
}

/// Failure while assembling a wiring map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WiringError {
    KindMismatch { a: PinId, b: PinId },
    DirectionMismatch { from: PinId, to: PinId },
    AnalogMultipleDrivers { net_pins: Vec<PinId> },
    PullOnAnalog { pin: PinId },
    ConflictingPull { pin: PinId },
    UnknownPin { pin: PinId },
    TypedAlreadyWired { port: PortId },
}

impl fmt::Display for WiringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KindMismatch { a, b } => {
                write!(f, "digital/analog mix pin {} and {}", a.0, b.0)
            }
            Self::DirectionMismatch { from, to } => {
                write!(f, "wire {} → {} has no source→sink direction", from.0, to.0)
            }
            Self::AnalogMultipleDrivers { net_pins } => {
                write!(f, "analog net has multiple drivers: {net_pins:?}")
            }
            Self::PullOnAnalog { pin } => write!(f, "pull on analog pin {}", pin.0),
            Self::ConflictingPull { pin } => write!(f, "conflicting pull on pin {}", pin.0),
            Self::UnknownPin { pin } => write!(f, "unknown pin {}", pin.0),
            Self::TypedAlreadyWired { port } => {
                write!(f, "typed port {} already on another wire", port.0)
            }
        }
    }
}

impl std::error::Error for WiringError {}

/// Failure to drive a pin or typed port after build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveError {
    UnknownPin { pin: PinId },
    NotADriver { pin: PinId },
    KindMismatch { pin: PinId },
    Conflict { a: DigitalLevel, b: DigitalLevel },
    AnalogMultipleDrivers { pin: PinId },
    UnknownPort { port: PortId },
}

impl fmt::Display for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPin { pin } => write!(f, "unknown pin {}", pin.0),
            Self::NotADriver { pin } => write!(f, "pin {} cannot drive", pin.0),
            Self::KindMismatch { pin } => write!(f, "signal kind mismatch on pin {}", pin.0),
            Self::Conflict { a, b } => write!(f, "digital conflict {a:?} vs {b:?}"),
            Self::AnalogMultipleDrivers { pin } => {
                write!(f, "analog multi-driver from pin {}", pin.0)
            }
            Self::UnknownPort { port } => write!(f, "unknown port {}", port.0),
        }
    }
}

impl std::error::Error for DriveError {}

/// Failure to sense a resolved net or typed slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SenseError {
    UnknownPin { pin: PinId },
    KindMismatch { pin: PinId },
    AnalogUndriven { pin: PinId },
    Conflict,
    UnknownPort { port: PortId },
    UndrivenPort { port: PortId },
}

impl fmt::Display for SenseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPin { pin } => write!(f, "unknown pin {}", pin.0),
            Self::KindMismatch { pin } => write!(f, "signal kind mismatch on pin {}", pin.0),
            Self::AnalogUndriven { pin } => write!(f, "analog pin {} undriven", pin.0),
            Self::Conflict => write!(f, "digital net in conflict"),
            Self::UnknownPort { port } => write!(f, "unknown port {}", port.0),
            Self::UndrivenPort { port } => write!(f, "typed port {} undriven", port.0),
        }
    }
}

impl std::error::Error for SenseError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct NetId(u32);

#[derive(Clone, Copy)]
struct PendingPin {
    kind: SignalKind,
    role: PinRole,
}

#[derive(Clone, Copy)]
struct TypedEdge {
    src: PortId,
    sink: PortId,
    ty: TypeId,
}

/// Immutable-style interconnect construction.
#[derive(Default)]
pub struct WiringBuilder {
    pins: HashMap<PinId, PendingPin>,
    parent: HashMap<PinId, PinId>,
    pulls: HashMap<PinId, Pull>,
    typed: Vec<TypedEdge>,
}

impl WiringBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_pin(&mut self, pin: PinRef) -> Result<(), WiringError> {
        match self.pins.get(&pin.id) {
            Some(existing) if existing.kind != pin.kind || existing.role != pin.role => {
                Err(WiringError::KindMismatch {
                    a: pin.id,
                    b: pin.id,
                })
            }
            Some(_) => Ok(()),
            None => {
                self.pins.insert(
                    pin.id,
                    PendingPin {
                        kind: pin.kind,
                        role: pin.role,
                    },
                );
                self.parent.insert(pin.id, pin.id);
                Ok(())
            }
        }
    }

    fn find(&mut self, id: PinId) -> PinId {
        let mut cur = id;
        let mut hops = Vec::new();
        loop {
            let p = *self.parent.get(&cur).expect("pin in parent");
            if p == cur {
                break;
            }
            hops.push(cur);
            cur = p;
        }
        for h in hops {
            self.parent.insert(h, cur);
        }
        cur
    }

    fn union(&mut self, a: PinId, b: PinId) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent.insert(rb, ra);
        }
    }

    /// Unidirectional wire from `from` (source/bidi) to `to` (sink/bidi).
    /// Digital connections sharing a pin become one net.
    pub fn connect(mut self, from: PinRef, to: PinRef) -> Result<Self, WiringError> {
        self.ensure_pin(from)?;
        self.ensure_pin(to)?;
        if from.kind != to.kind {
            return Err(WiringError::KindMismatch {
                a: from.id,
                b: to.id,
            });
        }
        if !from.can_drive() || !to.can_sense() {
            return Err(WiringError::DirectionMismatch {
                from: from.id,
                to: to.id,
            });
        }
        self.union(from.id, to.id);
        Ok(self)
    }

    pub fn pull(mut self, pin: PinRef, pull: Pull) -> Result<Self, WiringError> {
        self.ensure_pin(pin)?;
        if pin.kind != SignalKind::Digital {
            return Err(WiringError::PullOnAnalog { pin: pin.id });
        }
        if let Some(existing) = self.pulls.get(&pin.id) {
            if *existing != pull {
                return Err(WiringError::ConflictingPull { pin: pin.id });
            }
        }
        self.pulls.insert(pin.id, pull);
        Ok(self)
    }

    /// Point-to-point (or 1:N fan-out) typed wire. Compile-time `T` must match.
    pub fn connect_typed<T: 'static>(
        mut self,
        from: SourcePort<T>,
        to: SinkPort<T>,
    ) -> Result<Self, WiringError> {
        self.typed.push(TypedEdge {
            src: from.id,
            sink: to.id,
            ty: TypeId::of::<T>(),
        });
        Ok(self)
    }

    pub fn build(mut self) -> Result<Interconnect, WiringError> {
        let mut nets: HashMap<NetId, NetState> = HashMap::new();
        let mut pin_net: HashMap<PinId, NetId> = HashMap::new();
        let mut pin_meta: HashMap<PinId, PendingPin> = HashMap::new();
        let mut next_net = 1u32;

        let ids: Vec<PinId> = self.pins.keys().copied().collect();
        for id in ids {
            let root = self.find(id);
            let meta = *self.pins.get(&id).expect("pin");
            pin_meta.insert(id, meta);
            let net_id = if let Some(existing) = pin_net.get(&root) {
                *existing
            } else {
                let nid = NetId(next_net);
                next_net += 1;
                pin_net.insert(root, nid);
                nid
            };
            pin_net.insert(id, net_id);
        }

        for (id, meta) in &pin_meta {
            let nid = pin_net[id];
            let net = nets.entry(nid).or_insert_with(|| match meta.kind {
                SignalKind::Digital => NetState::Digital(DigitalNet {
                    pull: None,
                    drivers: Vec::new(),
                    drives: HashMap::new(),
                    resolved: DigitalLevel::HiZ,
                    conflict: false,
                }),
                SignalKind::Analog => NetState::Analog(AnalogNet {
                    driver: None,
                    value: None,
                }),
            });
            match (net, meta.kind, meta.role) {
                (NetState::Digital(_), SignalKind::Analog, _)
                | (NetState::Analog(_), SignalKind::Digital, _) => {
                    return Err(WiringError::KindMismatch { a: *id, b: *id });
                }
                (NetState::Digital(d), SignalKind::Digital, role) => {
                    if matches!(role, PinRole::Source | PinRole::Bidirectional) {
                        d.drivers.push(*id);
                        d.drives.insert(*id, DigitalLevel::HiZ);
                    }
                }
                (
                    NetState::Analog(a),
                    SignalKind::Analog,
                    PinRole::Source | PinRole::Bidirectional,
                ) => {
                    if let Some(prev) = a.driver {
                        return Err(WiringError::AnalogMultipleDrivers {
                            net_pins: vec![prev, *id],
                        });
                    }
                    a.driver = Some(*id);
                }
                (NetState::Analog(_), SignalKind::Analog, PinRole::Sink) => {}
            }
        }

        for (pin, pull) in &self.pulls {
            let nid = *pin_net
                .get(pin)
                .ok_or(WiringError::UnknownPin { pin: *pin })?;
            match nets.get_mut(&nid) {
                Some(NetState::Digital(d)) => {
                    if let Some(existing) = d.pull {
                        if existing != *pull {
                            return Err(WiringError::ConflictingPull { pin: *pin });
                        }
                    }
                    d.pull = Some(*pull);
                    d.resolve_idle();
                }
                Some(NetState::Analog(_)) => {
                    return Err(WiringError::PullOnAnalog { pin: *pin });
                }
                None => return Err(WiringError::UnknownPin { pin: *pin }),
            }
        }

        let mut analog_drivers: HashMap<NetId, Vec<PinId>> = HashMap::new();
        for (id, meta) in &pin_meta {
            if meta.kind == SignalKind::Analog && meta.can_drive() {
                analog_drivers.entry(pin_net[id]).or_default().push(*id);
            }
        }
        for pins in analog_drivers.values() {
            if pins.len() > 1 {
                return Err(WiringError::AnalogMultipleDrivers {
                    net_pins: pins.clone(),
                });
            }
        }

        let mut typed_nets: HashMap<PortId, TypedNet> = HashMap::new();
        let mut sink_of: HashMap<PortId, PortId> = HashMap::new();
        let mut src_key: HashMap<PortId, PortId> = HashMap::new();

        for edge in &self.typed {
            let key = *src_key.entry(edge.src).or_insert(edge.src);
            if let Some(net) = typed_nets.get(&key) {
                if net.ty != edge.ty {
                    return Err(WiringError::KindMismatch {
                        a: PinId(edge.src.0),
                        b: PinId(edge.sink.0),
                    });
                }
            }
            if let Some(prev_src) = sink_of.get(&edge.sink) {
                if *prev_src != key {
                    return Err(WiringError::TypedAlreadyWired { port: edge.sink });
                }
            }
            sink_of.insert(edge.sink, key);
            typed_nets.entry(key).or_insert_with(|| TypedNet {
                ty: edge.ty,
                value: None,
            });
        }

        Ok(Interconnect {
            pin_net,
            pin_meta,
            nets,
            typed_by_src: typed_nets,
            typed_sink_src: sink_of,
        })
    }
}

impl PendingPin {
    fn can_drive(self) -> bool {
        matches!(self.role, PinRole::Source | PinRole::Bidirectional)
    }
}

struct DigitalNet {
    pull: Option<Pull>,
    drivers: Vec<PinId>,
    drives: HashMap<PinId, DigitalLevel>,
    resolved: DigitalLevel,
    conflict: bool,
}

impl DigitalNet {
    fn recompute(&mut self) -> Result<(), DriveError> {
        let mut active: Vec<DigitalLevel> = Vec::new();
        for id in &self.drivers {
            match self.drives.get(id).copied().unwrap_or(DigitalLevel::HiZ) {
                DigitalLevel::HiZ => {}
                lvl => active.push(lvl),
            }
        }
        if active.is_empty() {
            self.conflict = false;
            self.resolved = match self.pull {
                Some(Pull::Up) => DigitalLevel::High,
                Some(Pull::Down) => DigitalLevel::Low,
                None => DigitalLevel::HiZ,
            };
            return Ok(());
        }
        let first = active[0];
        if active.iter().all(|l| *l == first) {
            self.conflict = false;
            self.resolved = first;
            Ok(())
        } else {
            self.conflict = true;
            let other = active.into_iter().find(|l| *l != first).unwrap();
            Err(DriveError::Conflict { a: first, b: other })
        }
    }

    fn resolve_idle(&mut self) {
        let _ = self.recompute();
    }
}

struct AnalogNet {
    driver: Option<PinId>,
    value: Option<AnalogVoltage>,
}

enum NetState {
    Digital(DigitalNet),
    Analog(AnalogNet),
}

struct TypedNet {
    ty: TypeId,
    value: Option<Box<dyn Any + Send>>,
}

/// Finished interconnect owned by [`crate::Machine`].
pub struct Interconnect {
    pin_net: HashMap<PinId, NetId>,
    pin_meta: HashMap<PinId, PendingPin>,
    nets: HashMap<NetId, NetState>,
    typed_by_src: HashMap<PortId, TypedNet>,
    typed_sink_src: HashMap<PortId, PortId>,
}

impl Interconnect {
    #[must_use]
    pub fn empty() -> Self {
        WiringBuilder::new().build().expect("empty wiring is valid")
    }

    pub fn drive_digital(&mut self, pin: PinRef, level: DigitalLevel) -> Result<(), DriveError> {
        self.check_drive(pin, SignalKind::Digital)?;
        let nid = self.pin_net[&pin.id];
        match self.nets.get_mut(&nid) {
            Some(NetState::Digital(d)) => {
                d.drives.insert(pin.id, level);
                d.recompute()
            }
            _ => Err(DriveError::KindMismatch { pin: pin.id }),
        }
    }

    pub fn sense_digital(&self, pin: PinRef) -> Result<DigitalLevel, SenseError> {
        self.check_sense(pin, SignalKind::Digital)?;
        let nid = self.pin_net[&pin.id];
        match self.nets.get(&nid) {
            Some(NetState::Digital(d)) if d.conflict => Err(SenseError::Conflict),
            Some(NetState::Digital(d)) => Ok(d.resolved),
            _ => Err(SenseError::KindMismatch { pin: pin.id }),
        }
    }

    pub fn drive_analog(&mut self, pin: PinRef, volts: AnalogVoltage) -> Result<(), DriveError> {
        self.check_drive(pin, SignalKind::Analog)?;
        let nid = self.pin_net[&pin.id];
        match self.nets.get_mut(&nid) {
            Some(NetState::Analog(a)) => {
                if let Some(owner) = a.driver {
                    if owner != pin.id && a.value.is_some() {
                        return Err(DriveError::AnalogMultipleDrivers { pin: pin.id });
                    }
                }
                a.driver = Some(pin.id);
                a.value = Some(volts);
                Ok(())
            }
            _ => Err(DriveError::KindMismatch { pin: pin.id }),
        }
    }

    pub fn sense_analog(&self, pin: PinRef) -> Result<AnalogVoltage, SenseError> {
        self.check_sense(pin, SignalKind::Analog)?;
        let nid = self.pin_net[&pin.id];
        match self.nets.get(&nid) {
            Some(NetState::Analog(a)) => a.value.ok_or(SenseError::AnalogUndriven { pin: pin.id }),
            _ => Err(SenseError::KindMismatch { pin: pin.id }),
        }
    }

    pub fn drive_typed<T: Clone + Send + 'static>(
        &mut self,
        port: SourcePort<T>,
        value: T,
    ) -> Result<(), DriveError> {
        let net = self
            .typed_by_src
            .get_mut(&port.id)
            .ok_or(DriveError::UnknownPort { port: port.id })?;
        if net.ty != TypeId::of::<T>() {
            return Err(DriveError::UnknownPort { port: port.id });
        }
        net.value = Some(Box::new(value));
        Ok(())
    }

    pub fn sense_typed<T: Clone + Send + 'static>(
        &self,
        port: SinkPort<T>,
    ) -> Result<T, SenseError> {
        let src = *self
            .typed_sink_src
            .get(&port.id)
            .ok_or(SenseError::UnknownPort { port: port.id })?;
        let net = self
            .typed_by_src
            .get(&src)
            .ok_or(SenseError::UnknownPort { port: port.id })?;
        if net.ty != TypeId::of::<T>() {
            return Err(SenseError::UnknownPort { port: port.id });
        }
        net.value
            .as_ref()
            .and_then(|v| v.downcast_ref::<T>().cloned())
            .ok_or(SenseError::UndrivenPort { port: port.id })
    }

    fn check_drive(&self, pin: PinRef, kind: SignalKind) -> Result<(), DriveError> {
        let meta = self
            .pin_meta
            .get(&pin.id)
            .ok_or(DriveError::UnknownPin { pin: pin.id })?;
        if meta.kind != kind {
            return Err(DriveError::KindMismatch { pin: pin.id });
        }
        if !meta.can_drive() {
            return Err(DriveError::NotADriver { pin: pin.id });
        }
        Ok(())
    }

    fn check_sense(&self, pin: PinRef, kind: SignalKind) -> Result<(), SenseError> {
        let meta = self
            .pin_meta
            .get(&pin.id)
            .ok_or(SenseError::UnknownPin { pin: pin.id })?;
        if meta.kind != kind {
            return Err(SenseError::KindMismatch { pin: pin.id });
        }
        Ok(())
    }
}

impl Default for Interconnect {
    fn default() -> Self {
        Self::empty()
    }
}

/// Test / scaffold digital driver (GPIO-like bidirectional pin).
#[derive(Clone, Copy, Debug)]
pub struct DummyDriver {
    pin: PinRef,
}

impl DummyDriver {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pin: PinRef {
                id: alloc_pin(),
                kind: SignalKind::Digital,
                role: PinRole::Bidirectional,
            },
        }
    }
}

impl Default for DummyDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl Endpoint for DummyDriver {
    fn pin(&self) -> PinRef {
        self.pin
    }
}

/// Test / scaffold digital probe (LED / sense-only).
#[derive(Clone, Copy, Debug)]
pub struct DummyProbe {
    pin: PinRef,
}

impl DummyProbe {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pin: PinRef {
                id: alloc_pin(),
                kind: SignalKind::Digital,
                role: PinRole::Sink,
            },
        }
    }
}

impl Default for DummyProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl Endpoint for DummyProbe {
    fn pin(&self) -> PinRef {
        self.pin
    }
}

/// Test analogue of [`DummyDriver`] for analog nets.
#[derive(Clone, Copy, Debug)]
pub struct DummyAnalogDriver {
    pin: PinRef,
}

impl DummyAnalogDriver {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pin: PinRef {
                id: alloc_pin(),
                kind: SignalKind::Analog,
                role: PinRole::Source,
            },
        }
    }
}

impl Default for DummyAnalogDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl Endpoint for DummyAnalogDriver {
    fn pin(&self) -> PinRef {
        self.pin
    }
}

/// Test analogue of [`DummyProbe`] for analog nets.
#[derive(Clone, Copy, Debug)]
pub struct DummyAnalogProbe {
    pin: PinRef,
}

impl DummyAnalogProbe {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pin: PinRef {
                id: alloc_pin(),
                kind: SignalKind::Analog,
                role: PinRole::Sink,
            },
        }
    }
}

impl Default for DummyAnalogProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl Endpoint for DummyAnalogProbe {
    fn pin(&self) -> PinRef {
        self.pin
    }
}

/// Typed source used to exercise Port/Wire without a real peripheral.
#[derive(Debug)]
pub struct DummySource<T> {
    port: SourcePort<T>,
}

impl<T> DummySource<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            port: SourcePort {
                id: alloc_port(),
                _ty: PhantomData,
            },
        }
    }

    #[must_use]
    pub fn port(&self) -> SourcePort<T> {
        self.port
    }
}

impl<T> Default for DummySource<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed sink used to exercise Port/Wire without a real peripheral.
#[derive(Debug)]
pub struct DummySink<T> {
    port: SinkPort<T>,
}

impl<T> DummySink<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            port: SinkPort {
                id: alloc_port(),
                _ty: PhantomData,
            },
        }
    }

    #[must_use]
    pub fn port(&self) -> SinkPort<T> {
        self.port
    }
}

impl<T> Default for DummySink<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_reaches_probe() {
        let src = DummyDriver::new();
        let led = DummyProbe::new();
        let mut ic = WiringBuilder::new()
            .connect(src.pin(), led.pin())
            .unwrap()
            .build()
            .unwrap();
        ic.drive_digital(src.pin(), DigitalLevel::High).unwrap();
        assert_eq!(ic.sense_digital(led.pin()).unwrap(), DigitalLevel::High);
        ic.drive_digital(src.pin(), DigitalLevel::Low).unwrap();
        assert_eq!(ic.sense_digital(led.pin()).unwrap(), DigitalLevel::Low);
    }

    #[test]
    fn pull_up_when_all_hiz() {
        let src = DummyDriver::new();
        let led = DummyProbe::new();
        let mut ic = WiringBuilder::new()
            .connect(src.pin(), led.pin())
            .unwrap()
            .pull(src.pin(), Pull::Up)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(ic.sense_digital(led.pin()).unwrap(), DigitalLevel::High);
        ic.drive_digital(src.pin(), DigitalLevel::Low).unwrap();
        assert_eq!(ic.sense_digital(led.pin()).unwrap(), DigitalLevel::Low);
        ic.drive_digital(src.pin(), DigitalLevel::HiZ).unwrap();
        assert_eq!(ic.sense_digital(led.pin()).unwrap(), DigitalLevel::High);
    }

    #[test]
    fn pull_down_when_all_hiz() {
        let src = DummyDriver::new();
        let led = DummyProbe::new();
        let ic = WiringBuilder::new()
            .connect(src.pin(), led.pin())
            .unwrap()
            .pull(led.pin(), Pull::Down)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(ic.sense_digital(led.pin()).unwrap(), DigitalLevel::Low);
    }

    #[test]
    fn digital_conflict_on_opposing_drives() {
        let a = DummyDriver::new();
        let b = DummyDriver::new();
        let mut ic = WiringBuilder::new()
            .connect(a.pin(), b.pin())
            .unwrap()
            .build()
            .unwrap();
        ic.drive_digital(a.pin(), DigitalLevel::High).unwrap();
        let err = ic.drive_digital(b.pin(), DigitalLevel::Low).unwrap_err();
        assert!(matches!(err, DriveError::Conflict { .. }));
        assert_eq!(ic.sense_digital(a.pin()).unwrap_err(), SenseError::Conflict);
    }

    #[test]
    fn mixed_kind_is_build_error() {
        let d = DummyDriver::new();
        let a = DummyAnalogProbe::new();
        assert!(matches!(
            WiringBuilder::new().connect(d.pin(), a.pin()),
            Err(WiringError::KindMismatch { .. })
        ));
    }

    #[test]
    fn analog_single_driver_and_sense() {
        let src = DummyAnalogDriver::new();
        let probe = DummyAnalogProbe::new();
        let mut ic = WiringBuilder::new()
            .connect(src.pin(), probe.pin())
            .unwrap()
            .build()
            .unwrap();
        ic.drive_analog(src.pin(), AnalogVoltage(3_300_000))
            .unwrap();
        assert_eq!(
            ic.sense_analog(probe.pin()).unwrap(),
            AnalogVoltage(3_300_000)
        );
    }

    #[test]
    fn analog_undriven_sense_fails() {
        let src = DummyAnalogDriver::new();
        let probe = DummyAnalogProbe::new();
        let ic = WiringBuilder::new()
            .connect(src.pin(), probe.pin())
            .unwrap()
            .build()
            .unwrap();
        assert!(matches!(
            ic.sense_analog(probe.pin()).unwrap_err(),
            SenseError::AnalogUndriven { .. }
        ));
    }

    #[test]
    fn analog_two_drivers_fail_at_build() {
        let a = DummyAnalogDriver::new();
        let b = DummyAnalogDriver::new();
        let p = DummyAnalogProbe::new();
        let built = WiringBuilder::new()
            .connect(a.pin(), p.pin())
            .unwrap()
            .connect(b.pin(), p.pin())
            .unwrap()
            .build();
        assert!(matches!(
            built,
            Err(WiringError::AnalogMultipleDrivers { .. })
        ));
    }

    #[test]
    fn sink_cannot_drive() {
        let src = DummyDriver::new();
        let led = DummyProbe::new();
        let mut ic = WiringBuilder::new()
            .connect(src.pin(), led.pin())
            .unwrap()
            .build()
            .unwrap();
        assert!(matches!(
            ic.drive_digital(led.pin(), DigitalLevel::High).unwrap_err(),
            DriveError::NotADriver { .. }
        ));
    }

    #[test]
    fn direction_mismatch_two_sinks() {
        let a = DummyProbe::new();
        let b = DummyProbe::new();
        assert!(matches!(
            WiringBuilder::new().connect(a.pin(), b.pin()),
            Err(WiringError::DirectionMismatch { .. })
        ));
    }

    #[test]
    fn typed_port_broadcasts() {
        let tx = DummySource::<u8>::new();
        let rx_a = DummySink::<u8>::new();
        let rx_b = DummySink::<u8>::new();
        let mut ic = WiringBuilder::new()
            .connect_typed(tx.port(), rx_a.port())
            .unwrap()
            .connect_typed(tx.port(), rx_b.port())
            .unwrap()
            .build()
            .unwrap();
        ic.drive_typed(tx.port(), 0x5A).unwrap();
        assert_eq!(ic.sense_typed(rx_a.port()).unwrap(), 0x5A);
        assert_eq!(ic.sense_typed(rx_b.port()).unwrap(), 0x5A);
    }

    #[test]
    fn empty_interconnect_builds() {
        let _ = Interconnect::empty();
    }
}
