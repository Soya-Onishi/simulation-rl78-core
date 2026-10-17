//! Named board devices and the actions they expose.
//!
//! Callers look up a device by [`id_hash`] of its name, then an action by the
//! hash of its id. Parameter values travel as [`Parameters`]; they are not the
//! lookup key. Every process hashes the same UTF-8 strings with [`id_hash`],
//! so a node, an arbiter, and a server agree without exchanging the hash in JSON.

use std::collections::HashMap;

use xxhash_rust::xxh3::xxh3_64;

use crate::clock::Tick;
use crate::reset::Resettable;

/// Largest number of parameters one action accepts.
pub const MAX_ACTION_PARAMS: usize = 8;

/// Stable 64-bit hash of an id string (UTF-8 bytes), `xxh3_64`.
#[must_use]
pub fn id_hash(name: &str) -> u64 {
    xxh3_64(name.as_bytes())
}

/// One numeric parameter an action accepts.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamSpec {
    pub name: String,
    pub name_hash: u64,
    pub unit: Option<String>,
    pub description: Option<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub default: Option<f64>,
}

impl ParamSpec {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let name_hash = id_hash(&name);
        Self {
            name,
            name_hash,
            unit: None,
            description: None,
            min: None,
            max: None,
            default: None,
        }
    }
}

/// Validated parameter values passed into [`Action::execute`].
#[derive(Clone, Debug, PartialEq)]
pub struct Parameters {
    values: Vec<(u64, f64)>,
}

impl Parameters {
    #[must_use]
    pub fn get(&self, name_hash: u64) -> Option<f64> {
        self.values
            .iter()
            .find(|(hash, _)| *hash == name_hash)
            .map(|(_, value)| *value)
    }
}

/// Static description of one action, including its parameter schema.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionSpec {
    pub id: String,
    pub hash: u64,
    pub description: String,
    pub params: Vec<ParamSpec>,
}

impl ActionSpec {
    #[must_use]
    pub fn new(id: impl Into<String>, description: impl Into<String>) -> Self {
        let id = id.into();
        let hash = id_hash(&id);
        Self {
            id,
            hash,
            description: description.into(),
            params: Vec::new(),
        }
    }
}

/// Why an action was not applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionFailure {
    /// No device is registered under this hash.
    NotFoundDevice,
    /// The device has no action under this hash.
    NotFoundAction,
    /// This device already has an action in progress.
    Busy,
    /// A parameter is missing, unknown, or out of range.
    BadParam,
}

/// One invocable operation owned by a [`Device`].
pub trait Action: Send {
    fn spec(&self) -> &ActionSpec;
    fn execute(&mut self, now: Tick, params: &Parameters) -> Result<(), ActionFailure>;
    /// Release effects whose virtual-time deadline has arrived.
    fn poll(&mut self, now: Tick);
    fn busy(&self, now: Tick) -> bool;
    fn reset_action(&mut self);
}

/// Logical device owned by [`crate::Machine`] (SoC part, board peripheral, …).
///
/// A board device such as a switch owns [`Action`]s and is looked up by
/// [`id_hash`] of its name. Parts with no actions keep the default methods.
pub trait Device: Send + Resettable {
    fn name(&self) -> &str {
        ""
    }

    fn name_hash(&self) -> u64 {
        0
    }

    fn poll(&mut self, _now: Tick) {}

    fn apply(
        &mut self,
        _action_hash: u64,
        _now: Tick,
        _raw: &[(u64, f64)],
    ) -> Result<(), ActionFailure> {
        Err(ActionFailure::NotFoundAction)
    }

    fn action_mut(&mut self, _action_hash: u64) -> Option<&mut (dyn Action + '_)> {
        None
    }

    fn action_specs(&self) -> Vec<ActionSpec> {
        Vec::new()
    }
}

/// Failure while registering a [`Device`].
#[derive(Debug, PartialEq, Eq)]
pub enum DeviceError {
    EmptyName,
    DuplicateDevice { name: String },
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => write!(f, "device name is empty"),
            Self::DuplicateDevice { name } => write!(f, "duplicate device '{name}'"),
        }
    }
}

impl std::error::Error for DeviceError {}

/// One row of a device's action list, for advertisement outside the kernel.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionCatalogEntry {
    pub device: String,
    pub device_hash: u64,
    pub action: String,
    pub action_hash: u64,
    pub description: String,
    pub params: Vec<ParamSpec>,
}

/// Check `raw` against `spec` and fill defaults.
///
/// # Errors
///
/// Returns [`ActionFailure::BadParam`] when a name is unknown, duplicated,
/// missing without a default, or outside the spec.
pub fn prepare_parameters(
    spec: &ActionSpec,
    raw: &[(u64, f64)],
) -> Result<Parameters, ActionFailure> {
    if raw.len() > MAX_ACTION_PARAMS {
        return Err(ActionFailure::BadParam);
    }
    let mut seen = HashMap::new();
    for (hash, value) in raw {
        if seen.insert(*hash, *value).is_some() {
            return Err(ActionFailure::BadParam);
        }
        if !spec.params.iter().any(|param| param.name_hash == *hash) {
            return Err(ActionFailure::BadParam);
        }
    }
    let mut values = Vec::with_capacity(spec.params.len());
    for param in &spec.params {
        let value = match seen.remove(&param.name_hash) {
            Some(value) => value,
            None => param.default.ok_or(ActionFailure::BadParam)?,
        };
        if !value.is_finite() {
            return Err(ActionFailure::BadParam);
        }
        if let Some(min) = param.min
            && value < min
        {
            return Err(ActionFailure::BadParam);
        }
        if let Some(max) = param.max
            && value > max
        {
            return Err(ActionFailure::BadParam);
        }
        values.push((param.name_hash, value));
    }
    if !seen.is_empty() {
        return Err(ActionFailure::BadParam);
    }
    Ok(Parameters { values })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_hash_is_stable_and_separates_names() {
        assert_eq!(id_hash("sw1"), id_hash("sw1"));
        assert_ne!(id_hash("sw1"), id_hash("hold"));
        assert_ne!(id_hash("ab"), id_hash("a"));
    }
}
