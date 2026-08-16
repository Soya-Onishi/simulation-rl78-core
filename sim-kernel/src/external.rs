//! Off-board endpoints for later inter-process wiring.
//!
//! [`ExternalSource`] and [`ExternalSink`] are the board-edge counterparts of
//! [`SourcePort`](crate::SourcePort) and a wire sink closure. Socket I/O is out
//! of scope; these types only assign identities and shuttle values across the
//! existing in-process [`Wire`](crate::Wire) graph.

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::SourcePort;

/// Identifier assigned to an [`ExternalSource`] or [`ExternalSink`].
///
/// IDs are unique for the lifetime of the process and are never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExternalId(u32);

impl ExternalId {
    #[must_use]
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

/// Failure to construct an external endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExternalError {
    /// `name` is not an ASCII Python identifier, or is a Python keyword.
    InvalidName { name: String },
    /// Another live endpoint already uses `name`.
    DuplicateName { name: String },
}

impl fmt::Display for ExternalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name } => {
                write!(
                    f,
                    "external endpoint name {name:?} is not a Python identifier"
                )
            }
            Self::DuplicateName { name } => {
                write!(f, "external endpoint name {name:?} is already in use")
            }
        }
    }
}

impl std::error::Error for ExternalError {}

fn next_id() -> ExternalId {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    ExternalId(NEXT.fetch_add(1, Ordering::Relaxed))
}

fn name_set() -> &'static Mutex<HashSet<String>> {
    static NAMES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    NAMES.get_or_init(|| Mutex::new(HashSet::new()))
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

struct NameLease {
    name: String,
}

impl NameLease {
    fn claim(name: String) -> Result<Self, ExternalError> {
        if !is_ascii_python_identifier(&name) {
            return Err(ExternalError::InvalidName { name });
        }
        let mut names = name_set().lock().expect("external names");
        if !names.insert(name.clone()) {
            return Err(ExternalError::DuplicateName { name });
        }
        Ok(Self { name })
    }

    fn as_str(&self) -> &str {
        &self.name
    }
}

impl Drop for NameLease {
    fn drop(&mut self) {
        name_set()
            .lock()
            .expect("external names")
            .remove(&self.name);
    }
}

/// Board-edge source: host input is injected with [`Self::receive`], which
/// [`SourcePort::drive`]s the bound in-board wire.
pub struct ExternalSource<T> {
    id: ExternalId,
    name: NameLease,
    port: SourcePort<T>,
}

impl<T> ExternalSource<T> {
    pub fn new(name: impl Into<String>) -> Result<Self, ExternalError> {
        Ok(Self {
            id: next_id(),
            name: NameLease::claim(name.into())?,
            port: SourcePort::new(),
        })
    }

    #[must_use]
    pub fn id(&self) -> ExternalId {
        self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    #[must_use]
    pub fn port(&self) -> &SourcePort<T> {
        &self.port
    }

    pub fn port_mut(&mut self) -> &mut SourcePort<T> {
        &mut self.port
    }
}

impl<T: Send + 'static> ExternalSource<T> {
    /// Injects a value as if it arrived from off-board IPC.
    pub fn receive(&self, value: T) {
        self.port.drive(value);
    }
}

struct SinkSnapshot<T> {
    values: Vec<T>,
    changed: Option<usize>,
}

struct SinkInner<T> {
    id: ExternalId,
    name: NameLease,
    snapshot: Mutex<SinkSnapshot<T>>,
}

/// Board-edge sink: in-board `drive` updates a snapshot of every source value.
///
/// Clone this handle into the `'static` closure passed to [`crate::Wire::sink`].
pub struct ExternalSink<T> {
    inner: Arc<SinkInner<T>>,
}

impl<T> Clone for ExternalSink<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> ExternalSink<T> {
    pub fn new(name: impl Into<String>) -> Result<Self, ExternalError> {
        Ok(Self {
            inner: Arc::new(SinkInner {
                id: next_id(),
                name: NameLease::claim(name.into())?,
                snapshot: Mutex::new(SinkSnapshot {
                    values: Vec::new(),
                    changed: None,
                }),
            }),
        })
    }

    #[must_use]
    pub fn id(&self) -> ExternalId {
        self.inner.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.inner.name.as_str()
    }

    /// Index of the source that last called this sink, if any.
    #[must_use]
    pub fn last_changed(&self) -> Option<usize> {
        self.inner.snapshot.lock().expect("external sink").changed
    }
}

impl<T: Clone> ExternalSink<T> {
    /// Records every source value from a [`crate::Wire`] sink callback.
    pub fn sink(&self, values: &[T], changed: usize) {
        let mut g = self.inner.snapshot.lock().expect("external sink");
        g.values = values.to_vec();
        g.changed = Some(changed);
    }

    /// Latest snapshot of all source values (empty until the first `sink` call).
    #[must_use]
    pub fn values(&self) -> Vec<T> {
        self.inner
            .snapshot
            .lock()
            .expect("external sink")
            .values
            .clone()
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
                matches!(
                    ExternalSink::<u8>::new(name),
                    Err(ExternalError::InvalidName { .. })
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn accepts_ascii_identifiers() {
        let a = ExternalSink::<u8>::new("uart_tx").unwrap();
        assert_eq!(a.name(), "uart_tx");
        drop(a);
        let _b = ExternalSource::<u8>::new("_hidden").unwrap();
    }

    #[test]
    fn duplicate_name_is_rejected_until_drop() {
        let a = ExternalSink::<u8>::new("dup_endpoint").unwrap();
        assert!(matches!(
            ExternalSource::<u8>::new("dup_endpoint"),
            Err(ExternalError::DuplicateName { .. })
        ));
        drop(a);
        let _b = ExternalSource::<u8>::new("dup_endpoint").unwrap();
    }

    #[test]
    fn ids_are_unique() {
        let a = ExternalSink::<u8>::new("id_left").unwrap();
        let b = ExternalSource::<u8>::new("id_right").unwrap();
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn receive_drives_in_board_wire() {
        let mut ext = ExternalSource::<u8>::new("ext_src_in").unwrap();
        let sink = ExternalSink::<u8>::new("ext_src_tap").unwrap();
        let _w = Wire::new().source(ext.port_mut()).sink({
            let sink = sink.clone();
            move |values, changed| sink.sink(values, changed)
        });
        ext.receive(0x5A);
        assert_eq!(sink.values(), vec![0x5A]);
        assert_eq!(sink.last_changed(), Some(0));
    }

    #[test]
    fn sink_keeps_all_source_values() {
        let mut s0 = SourcePort::<u8>::new();
        let mut s1 = SourcePort::<u8>::new();
        let ext = ExternalSink::<u8>::new("ext_fan_in").unwrap();
        let _w = Wire::new()
            .sink({
                let ext = ext.clone();
                move |values, changed| ext.sink(values, changed)
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
}
