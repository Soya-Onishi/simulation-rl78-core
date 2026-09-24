//! Board-edge in/out ports for later inter-process wiring.
//!
//! [`InPort`] injects host/IPC values into the in-board [`Wire`](crate::Wire)
//! graph; [`OutPort`] captures guest-driven wire values for TX. Names align with
//! topology endpoint direction (`In` / `Out`), not wire Source/Sink terminology.
//!
//! Endpoint-name uniqueness is scoped to a [`BoardPorts`] instance (not process
//! global). Drop the board (`BoardPorts` + wired machine / data-plane clones) to
//! rebuild with the same names. Socket I/O is out of scope.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::SourcePort;

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
/// board. Clone the wired [`SourcePort`] via [`Self::port`] for data-plane RX.
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

    /// Re-queues a value previously taken from [`Self::drain_pending`] (e.g. after
    /// a transient IPC send failure).
    pub fn push_pending(&self, value: T) {
        self.inner
            .pending
            .lock()
            .expect("out port pending")
            .push(value);
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
/// Endpoint names are unique within this board only. Keys are
/// `(TypeId::of::<T>(), endpoint name)` so UART / digital / … can share one map.
#[derive(Default)]
pub struct BoardPorts {
    claimed_names: HashSet<String>,
    inputs: HashMap<(TypeId, String), Box<dyn Any>>,
    outputs: HashMap<(TypeId, String), Box<dyn Any>>,
    pending_clearers: Vec<Box<dyn Fn() + Send + Sync>>,
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

    pub fn insert_in<T: 'static>(&mut self, port: InPort<T>) -> Result<(), PortError> {
        self.claim_name(port.name())?;
        let key = (TypeId::of::<T>(), port.name().to_string());
        self.inputs.insert(key, Box::new(port));
        Ok(())
    }

    pub fn insert_out<T: Clone + Send + Sync + 'static>(
        &mut self,
        port: OutPort<T>,
    ) -> Result<(), PortError> {
        self.claim_name(port.name())?;
        let clearer = {
            let port = port.clone();
            Box::new(move || port.clear_pending()) as Box<dyn Fn() + Send + Sync>
        };
        self.pending_clearers.push(clearer);
        let key = (TypeId::of::<T>(), port.name().to_string());
        self.outputs.insert(key, Box::new(port));
        Ok(())
    }

    #[must_use]
    pub fn in_port<T: 'static>(&self, name: &str) -> Option<&InPort<T>> {
        self.inputs
            .get(&(TypeId::of::<T>(), name.to_string()))
            .and_then(|boxed| boxed.downcast_ref())
    }

    #[must_use]
    pub fn out_port<T: 'static>(&self, name: &str) -> Option<&OutPort<T>> {
        self.outputs
            .get(&(TypeId::of::<T>(), name.to_string()))
            .and_then(|boxed| boxed.downcast_ref())
    }

    /// Clears outbound pending queues on every registered out-port (guest reset).
    pub fn clear_pending(&self) {
        for clearer in &self.pending_clearers {
            clearer();
        }
    }

    /// Names of inserted in-ports of type `T`.
    #[must_use]
    pub fn in_port_names<T: 'static>(&self) -> Vec<&str> {
        let tid = TypeId::of::<T>();
        self.inputs
            .keys()
            .filter(|(t, _)| *t == tid)
            .map(|(_, name)| name.as_str())
            .collect()
    }

    /// Names of inserted out-ports of type `T`.
    #[must_use]
    pub fn out_port_names<T: 'static>(&self) -> Vec<&str> {
        let tid = TypeId::of::<T>();
        self.outputs
            .keys()
            .filter(|(t, _)| *t == tid)
            .map(|(_, name)| name.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Wire;

    #[test]
    fn rejects_non_python_identifiers() {
        for name in ["", "1abc", "a-b", "foo.bar", "class", "yield"] {
            assert!(
                matches!(OutPort::<u8>::new(name), Err(PortError::InvalidName { .. })),
                "{name}"
            );
        }
    }

    #[test]
    fn accepts_ascii_identifiers() {
        let a = OutPort::<u8>::new("uart_tx").unwrap();
        assert_eq!(a.name(), "uart_tx");
        drop(a);
        let _b = InPort::<u8>::new("_hidden").unwrap();
    }

    #[test]
    fn same_name_allowed_until_board_insert() {
        let a = OutPort::<u8>::new("dup_endpoint").unwrap();
        let b = InPort::<u8>::new("dup_endpoint").unwrap();
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
                .insert_out(OutPort::<u8>::new("uart0_tx").unwrap())
                .unwrap();
            ports
                .insert_in(InPort::<u8>::new("uart0_rx").unwrap())
                .unwrap();
        }
        let mut ports = BoardPorts::new();
        ports
            .insert_out(OutPort::<u8>::new("uart0_tx").unwrap())
            .unwrap();
        ports
            .insert_in(InPort::<u8>::new("uart0_rx").unwrap())
            .unwrap();
        assert!(ports.out_port::<u8>("uart0_tx").is_some());
        assert!(ports.in_port::<u8>("uart0_rx").is_some());
    }

    #[test]
    fn separate_boards_may_reuse_endpoint_names() {
        let mut a = BoardPorts::new();
        let mut b = BoardPorts::new();
        a.insert_out(OutPort::<u8>::new("uart0_tx").unwrap())
            .unwrap();
        b.insert_out(OutPort::<u8>::new("uart0_tx").unwrap())
            .unwrap();
    }

    #[test]
    fn ids_are_unique() {
        let a = OutPort::<u8>::new("id_left").unwrap();
        let b = InPort::<u8>::new("id_right").unwrap();
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn receive_drives_in_board_wire() {
        let mut ext = InPort::<u8>::new("ext_src_in").unwrap();
        let sink = OutPort::<u8>::new("ext_src_tap").unwrap();
        let _w = Wire::new().source(ext.port_mut()).sink({
            let sink = sink.clone();
            move |values, changed| sink.on_input(values, changed)
        });
        ext.receive(0x5A);
        assert_eq!(sink.values(), vec![0x5A]);
        assert_eq!(sink.last_changed(), Some(0));
    }

    #[test]
    fn out_port_keeps_all_source_values() {
        let mut s0 = SourcePort::<u8>::new();
        let mut s1 = SourcePort::<u8>::new();
        let ext = OutPort::<u8>::new("ext_fan_in").unwrap();
        let _w = Wire::new()
            .sink({
                let ext = ext.clone();
                move |values, changed| ext.on_input(values, changed)
            })
            .source(&mut s0)
            .source(&mut s1);
        s0.drive(1);
        assert_eq!(ext.values(), vec![1, 0]);
        assert_eq!(ext.last_changed(), Some(0));
        s1.drive(2);
        assert_eq!(ext.values(), vec![1, 2]);
        assert_eq!(ext.last_changed(), Some(1));
    }

    #[test]
    fn out_port_queues_consecutive_drives_for_drain_pending() {
        let mut src = SourcePort::<u8>::new();
        let ext = OutPort::<u8>::new("ext_tx_queue").unwrap();
        let _w = Wire::new().source(&mut src).sink({
            let ext = ext.clone();
            move |values, changed| ext.on_input(values, changed)
        });
        src.drive(1);
        src.drive(2);
        src.drive(3);
        assert_eq!(ext.drain_pending(), vec![1, 2, 3]);
        assert!(ext.drain_pending().is_empty());
    }

    #[test]
    fn board_ports_lookup_by_type_and_name() {
        let mut ports = BoardPorts::new();
        ports
            .insert_out(OutPort::<u8>::new("uart0_tx").unwrap())
            .unwrap();
        ports
            .insert_in(InPort::<u8>::new("uart0_rx").unwrap())
            .unwrap();
        assert!(ports.out_port::<u8>("uart0_tx").is_some());
        assert!(ports.in_port::<u8>("uart0_rx").is_some());
        assert!(ports.out_port::<u16>("uart0_tx").is_none());

        ports.out_port::<u8>("uart0_tx").unwrap().on_input(&[9], 0);
        assert_eq!(
            ports.out_port::<u8>("uart0_tx").unwrap().drain_pending(),
            vec![9]
        );
        ports.out_port::<u8>("uart0_tx").unwrap().on_input(&[8], 0);
        ports.clear_pending();
        assert!(
            ports
                .out_port::<u8>("uart0_tx")
                .unwrap()
                .drain_pending()
                .is_empty()
        );
    }
}
