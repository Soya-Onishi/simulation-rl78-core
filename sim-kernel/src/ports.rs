//! Board-edge in/out ports for later inter-process wiring.
//!
//! [`InPort`] injects host/IPC values into the in-board [`Wire`](crate::Wire)
//! graph; [`OutPort`] captures guest-driven wire values for TX. Names align with
//! topology endpoint direction (`In` / `Out`), not wire Source/Sink terminology.
//!
//! Endpoint-name uniqueness is scoped to a [`BoardPorts`] instance (not process
//! global). Drop the board (`BoardPorts` + wired machine / lane clones) to
//! rebuild with the same names. Socket I/O is out of scope.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::SourcePort;
use crate::dataplane::{DataPlane, ExternPort};

/// Identifier assigned to an [`InPort`] or [`OutPort`].
///
/// IDs are unique for the lifetime of the process and are never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PortId(u32);

impl PortId {
    #[must_use]
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

/// Failure to construct or register a board-edge port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortError {
    /// `name` is not an ASCII Python identifier, or is a Python keyword.
    InvalidName { name: String },
    /// Another port on the same [`BoardPorts`] already uses `name`.
    DuplicateName { name: String },
    /// No port named `name` is registered on this board.
    UnknownName { name: String },
    /// `name` is not an [`InPort`] of the requested payload type.
    InPortTypeMismatch { name: String },
    /// `name` is not an [`OutPort`] of the requested payload type.
    OutPortTypeMismatch { name: String },
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name } => {
                write!(f, "board port name {name:?} is not a Python identifier")
            }
            Self::DuplicateName { name } => {
                write!(
                    f,
                    "board port name {name:?} is already in use on this board"
                )
            }
            Self::UnknownName { name } => {
                write!(
                    f,
                    "board port name {name:?} is not registered on this board"
                )
            }
            Self::InPortTypeMismatch { name } => {
                write!(
                    f,
                    "board port {name:?} is not an InPort of the requested type"
                )
            }
            Self::OutPortTypeMismatch { name } => {
                write!(
                    f,
                    "board port {name:?} is not an OutPort of the requested type"
                )
            }
        }
    }
}

impl std::error::Error for PortError {}

fn next_id() -> PortId {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    PortId(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Python 3 hard keywords (`keyword.iskeyword`). ASCII identifiers only.
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

fn is_ascii_python_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    !PYTHON_KEYWORDS.contains(&name)
}

fn validate_port_name(name: String) -> Result<String, PortError> {
    if !is_ascii_python_identifier(&name) {
        return Err(PortError::InvalidName { name });
    }
    Ok(name)
}

/// Board input port: host/IPC values enter via [`Self::receive`], which
/// [`SourcePort::drive`]s the bound in-board wire (topology direction `In`).
///
/// Register with [`BoardPorts::insert_in`] so the endpoint name is unique on that
/// board. After registration, borrow via [`BoardPorts::inport`] or inject through
/// [`BoardPorts::receive`]. [`Clone`] shares the same [`SourcePort`] when
/// board/plant wiring still needs a typed handle alongside the registered port.
#[derive(Clone)]
pub struct InPort<T> {
    id: PortId,
    name: String,
    port: SourcePort<T>,
}

impl<T> InPort<T> {
    pub fn new(name: impl Into<String>) -> Result<Self, PortError> {
        Ok(Self {
            id: next_id(),
            name: validate_port_name(name.into())?,
            port: SourcePort::new(),
        })
    }

    #[must_use]
    pub fn id(&self) -> PortId {
        self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn port(&self) -> &SourcePort<T> {
        &self.port
    }

    pub fn port_mut(&mut self) -> &mut SourcePort<T> {
        &mut self.port
    }
}

impl<T: Send + 'static> InPort<T> {
    /// Injects a value as if it arrived from off-board IPC.
    pub fn receive(&self, value: T) {
        self.port.drive(value);
    }
}

struct SinkSnapshot<T> {
    values: Vec<T>,
    changed: Option<usize>,
}

struct OutPortInner<T> {
    id: PortId,
    name: String,
    snapshot: Mutex<SinkSnapshot<T>>,
    pending: Mutex<Vec<T>>,
}

/// Board output port: in-board `drive` updates a snapshot for TX (topology `Out`).
///
/// [`Clone`] shares the TX queue with Wire closures and the data plane. Register
/// with [`BoardPorts::insert_out`]; drop the whole board (ports + clones) before
/// rebuilding with the same endpoint names.
#[derive(Clone)]
pub struct OutPort<T> {
    inner: Arc<OutPortInner<T>>,
}

impl<T> OutPort<T> {
    pub fn new(name: impl Into<String>) -> Result<Self, PortError> {
        Ok(Self {
            inner: Arc::new(OutPortInner {
                id: next_id(),
                name: validate_port_name(name.into())?,
                snapshot: Mutex::new(SinkSnapshot {
                    values: Vec::new(),
                    changed: None,
                }),
                pending: Mutex::new(Vec::new()),
            }),
        })
    }

    #[must_use]
    pub fn id(&self) -> PortId {
        self.inner.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    #[must_use]
    pub fn last_changed(&self) -> Option<usize> {
        self.inner.snapshot.lock().expect("out port").changed
    }

    pub fn clear_pending(&self) {
        self.inner.pending.lock().expect("out port pending").clear();
    }
}

impl<T: Clone> OutPort<T> {
    pub fn on_input(&self, values: &[T], changed: usize) {
        {
            let mut g = self.inner.snapshot.lock().expect("out port");
            g.values = values.to_vec();
            g.changed = Some(changed);
        }
        if let Some(frame) = values.get(changed) {
            self.inner
                .pending
                .lock()
                .expect("out port pending")
                .push(frame.clone());
        }
    }

    #[must_use]
    pub fn values(&self) -> Vec<T> {
        self.inner.snapshot.lock().expect("out port").values.clone()
    }

    #[must_use]
    pub fn pending(&self) -> Vec<T> {
        self.inner.pending.lock().expect("out port pending").clone()
    }

    #[must_use]
    pub fn drain_pending(&self) -> Vec<T> {
        std::mem::take(&mut *self.inner.pending.lock().expect("out port pending"))
    }
}

/// Type-erased board-edge ports assembled at machine construction.
///
/// Endpoint names are unique within this board. Values are [`ExternPort`]
/// trait objects keyed only by name (no `TypeId`).
#[derive(Default)]
pub struct BoardPorts {
    claimed_names: HashSet<String>,
    ports: HashMap<String, Box<dyn ExternPort>>,
}

impl BoardPorts {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn claim_name(&mut self, name: &str) -> Result<(), PortError> {
        if !self.claimed_names.insert(name.to_string()) {
            return Err(PortError::DuplicateName {
                name: name.to_string(),
            });
        }
        Ok(())
    }

    pub fn insert_in<T: DataPlane>(&mut self, port: InPort<T>) -> Result<(), PortError> {
        self.claim_name(port.name())?;
        self.ports.insert(port.name().to_string(), Box::new(port));
        Ok(())
    }

    pub fn insert_out<T: DataPlane>(&mut self, port: OutPort<T>) -> Result<(), PortError> {
        self.claim_name(port.name())?;
        self.ports.insert(port.name().to_string(), Box::new(port));
        Ok(())
    }

    /// Removes a port by name for [`ExternPort::open`].
    pub fn remove(&mut self, name: &str) -> Option<Box<dyn ExternPort>> {
        self.ports.remove(name)
    }

    /// Borrow a registered [`InPort`] by endpoint name.
    ///
    /// Upcasts `&dyn ExternPort` to [`std::any::Any`], then downcasts to
    /// [`InPort<T>`].
    pub fn inport<T: DataPlane>(&self, name: &str) -> Result<&InPort<T>, PortError> {
        use std::any::Any;

        let boxed = self.ports.get(name).ok_or_else(|| PortError::UnknownName {
            name: name.to_string(),
        })?;
        let port: &dyn ExternPort = &**boxed;
        let any: &dyn Any = port;
        any.downcast_ref::<InPort<T>>()
            .ok_or_else(|| PortError::InPortTypeMismatch {
                name: name.to_string(),
            })
    }

    /// Inject a value into a registered [`InPort`] by endpoint name.
    pub fn receive<T: DataPlane>(&self, name: &str, value: T) -> Result<(), PortError> {
        self.inport::<T>(name)?.receive(value);
        Ok(())
    }

    /// Borrow a registered [`OutPort`] by endpoint name.
    ///
    /// Upcasts `&dyn ExternPort` to [`std::any::Any`], then downcasts to
    /// [`OutPort<T>`].
    pub fn outport<T: DataPlane>(&self, name: &str) -> Result<&OutPort<T>, PortError> {
        use std::any::Any;

        let boxed = self.ports.get(name).ok_or_else(|| PortError::UnknownName {
            name: name.to_string(),
        })?;
        let port: &dyn ExternPort = &**boxed;
        let any: &dyn Any = port;
        any.downcast_ref::<OutPort<T>>()
            .ok_or_else(|| PortError::OutPortTypeMismatch {
                name: name.to_string(),
            })
    }

    /// Clears outbound pending queues on every registered out-port (guest reset).
    pub fn clear_pending(&self) {
        for port in self.ports.values() {
            port.clear_pending();
        }
    }

    /// Endpoint names currently registered.
    #[must_use]
    pub fn port_names(&self) -> Vec<&str> {
        self.ports.keys().map(|s| s.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DigitalLevel, Wire};

    #[test]
    fn rejects_non_python_identifiers() {
        for name in ["", "1abc", "a-b", "foo.bar", "class", "yield"] {
            assert!(
                matches!(
                    OutPort::<DigitalLevel>::new(name),
                    Err(PortError::InvalidName { .. })
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn accepts_ascii_identifiers() {
        let a = OutPort::<DigitalLevel>::new("uart_tx").unwrap();
        assert_eq!(a.name(), "uart_tx");
        drop(a);
        let _b = InPort::<DigitalLevel>::new("_hidden").unwrap();
    }

    #[test]
    fn same_name_allowed_until_board_insert() {
        let a = OutPort::<DigitalLevel>::new("dup_endpoint").unwrap();
        let b = InPort::<DigitalLevel>::new("dup_endpoint").unwrap();
        let mut ports = BoardPorts::new();
        ports.insert_out(a).unwrap();
        assert!(matches!(
            ports.insert_in(b),
            Err(PortError::DuplicateName { .. })
        ));
    }

    #[test]
    fn board_drop_allows_rebuild_with_same_names() {
        {
            let mut ports = BoardPorts::new();
            ports
                .insert_out(OutPort::<DigitalLevel>::new("uart0_tx").unwrap())
                .unwrap();
            ports
                .insert_in(InPort::<DigitalLevel>::new("uart0_rx").unwrap())
                .unwrap();
        }
        let mut ports = BoardPorts::new();
        ports
            .insert_out(OutPort::<DigitalLevel>::new("uart0_tx").unwrap())
            .unwrap();
        ports
            .insert_in(InPort::<DigitalLevel>::new("uart0_rx").unwrap())
            .unwrap();
        assert!(ports.port_names().contains(&"uart0_tx"));
        assert!(ports.port_names().contains(&"uart0_rx"));
    }

    #[test]
    fn separate_boards_may_reuse_endpoint_names() {
        let mut a = BoardPorts::new();
        let mut b = BoardPorts::new();
        a.insert_out(OutPort::<DigitalLevel>::new("uart0_tx").unwrap())
            .unwrap();
        b.insert_out(OutPort::<DigitalLevel>::new("uart0_tx").unwrap())
            .unwrap();
    }

    #[test]
    fn ids_are_unique() {
        let a = OutPort::<DigitalLevel>::new("id_left").unwrap();
        let b = InPort::<DigitalLevel>::new("id_right").unwrap();
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn receive_drives_in_board_wire() {
        let mut ext = InPort::<DigitalLevel>::new("ext_src_in").unwrap();
        let sink = OutPort::<DigitalLevel>::new("ext_src_tap").unwrap();
        let _w = Wire::new().source(ext.port_mut()).sink({
            let sink = sink.clone();
            move |values, changed| sink.on_input(values, changed)
        });
        ext.receive(DigitalLevel::High);
        assert_eq!(sink.values(), vec![DigitalLevel::High]);
        assert_eq!(sink.last_changed(), Some(0));
    }

    #[test]
    fn out_port_keeps_all_source_values() {
        let mut s0 = SourcePort::<DigitalLevel>::new();
        let mut s1 = SourcePort::<DigitalLevel>::new();
        let ext = OutPort::<DigitalLevel>::new("ext_fan_in").unwrap();
        let _w = Wire::new()
            .sink({
                let ext = ext.clone();
                move |values, changed| ext.on_input(values, changed)
            })
            .source(&mut s0)
            .source(&mut s1);
        s0.drive(DigitalLevel::High);
        assert_eq!(ext.values(), vec![DigitalLevel::High, DigitalLevel::HiZ]);
        assert_eq!(ext.last_changed(), Some(0));
        s1.drive(DigitalLevel::Low);
        assert_eq!(ext.values(), vec![DigitalLevel::High, DigitalLevel::Low]);
        assert_eq!(ext.last_changed(), Some(1));
    }

    #[test]
    fn out_port_queues_consecutive_drives_for_drain_pending() {
        let mut src = SourcePort::<DigitalLevel>::new();
        let ext = OutPort::<DigitalLevel>::new("ext_tx_queue").unwrap();
        let _w = Wire::new().source(&mut src).sink({
            let ext = ext.clone();
            move |values, changed| ext.on_input(values, changed)
        });
        src.drive(DigitalLevel::High);
        src.drive(DigitalLevel::Low);
        src.drive(DigitalLevel::HiZ);
        assert_eq!(
            ext.drain_pending(),
            vec![DigitalLevel::High, DigitalLevel::Low, DigitalLevel::HiZ]
        );
        assert!(ext.drain_pending().is_empty());
    }

    #[test]
    fn board_ports_remove_and_clear_pending() {
        let tx = OutPort::<DigitalLevel>::new("uart0_tx").unwrap();
        let tx_handle = tx.clone();
        let mut ports = BoardPorts::new();
        ports.insert_out(tx).unwrap();
        ports
            .insert_in(InPort::<DigitalLevel>::new("uart0_rx").unwrap())
            .unwrap();
        tx_handle.on_input(&[DigitalLevel::High], 0);
        ports.clear_pending();
        assert!(tx_handle.drain_pending().is_empty());
        assert!(ports.remove("uart0_tx").is_some());
        assert!(ports.remove("uart0_tx").is_none());
    }

    #[test]
    fn board_ports_receive_downcasts_in_port() {
        let mut ext = InPort::<DigitalLevel>::new("ext_rx").unwrap();
        let sink = OutPort::<DigitalLevel>::new("ext_tap").unwrap();
        let _w = Wire::new().source(ext.port_mut()).sink({
            let sink = sink.clone();
            move |values, changed| sink.on_input(values, changed)
        });
        let mut ports = BoardPorts::new();
        ports.insert_in(ext).unwrap();
        ports.receive("ext_rx", DigitalLevel::High).unwrap();
        assert_eq!(sink.values(), vec![DigitalLevel::High]);
    }

    #[test]
    fn board_ports_receive_rejects_unknown_and_type_mismatch() {
        let mut ports = BoardPorts::new();
        ports
            .insert_in(InPort::<DigitalLevel>::new("ext_rx").unwrap())
            .unwrap();
        ports
            .insert_out(OutPort::<DigitalLevel>::new("ext_tx").unwrap())
            .unwrap();
        assert!(matches!(
            ports.receive("missing", DigitalLevel::High),
            Err(PortError::UnknownName { .. })
        ));
        assert!(matches!(
            ports.receive("ext_tx", DigitalLevel::High),
            Err(PortError::InPortTypeMismatch { .. })
        ));
        assert!(matches!(
            ports.inport::<DigitalLevel>("ext_tx"),
            Err(PortError::InPortTypeMismatch { .. })
        ));
    }

    #[test]
    fn board_ports_outport_downcasts_out_port() {
        let mut src = SourcePort::<DigitalLevel>::new();
        let ext = OutPort::<DigitalLevel>::new("ext_tx").unwrap();
        let _w = Wire::new().source(&mut src).sink({
            let ext = ext.clone();
            move |values, changed| ext.on_input(values, changed)
        });
        let mut ports = BoardPorts::new();
        ports.insert_out(ext).unwrap();
        src.drive(DigitalLevel::High);
        let tx = ports.outport::<DigitalLevel>("ext_tx").unwrap();
        assert_eq!(tx.pending(), vec![DigitalLevel::High]);
        assert!(matches!(
            ports.outport::<DigitalLevel>("missing"),
            Err(PortError::UnknownName { .. })
        ));
        assert!(matches!(
            ports.outport::<crate::AnalogVoltage>("ext_tx"),
            Err(PortError::OutPortTypeMismatch { .. })
        ));
    }
}
