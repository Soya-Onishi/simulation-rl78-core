//! Action catalog and invoke samples shared by node, arbiter, and server.
//!
//! JSON carries names only. Each process derives the same [`sim_kernel::id_hash`]
//! from those strings. These samples are the non-JSON path and store the hashes.

use iceoryx2::prelude::ZeroCopySend;
use sim_kernel::{ActionCatalogEntry, ParamSpec, id_hash};

use crate::contract::{AdvertisedAction, AdvertisedActionParam};

/// iceoryx slots on the node request subscriber.
///
/// The node executes every request it drains. Samples that do not fit are
/// dropped by the transport, and those clients time out.
pub const ACTION_RECV_CAP: usize = 128;

pub const NAME_CAP: usize = 64;
pub const ACTION_DESC_CAP: usize = 128;
pub const UNIT_CAP: usize = 16;
pub const PARAM_DESC_CAP: usize = 64;

const FLAG_MIN: u8 = 1;
const FLAG_MAX: u8 = 2;
const FLAG_DEFAULT: u8 = 4;

/// Result status on the action result sample.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionStatus {
    Applied = 1,
    Stopped = 2,
    Busy = 3,
    NotFoundDevice = 4,
    BadParam = 5,
    NotFoundAction = 7,
}

impl ActionStatus {
    #[must_use]
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Applied,
            2 => Self::Stopped,
            3 => Self::Busy,
            4 => Self::NotFoundDevice,
            5 => Self::BadParam,
            7 => Self::NotFoundAction,
            _ => return None,
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, ZeroCopySend)]
pub struct ActionParam {
    pub name: [u8; NAME_CAP],
    pub name_len: u8,
    pub name_hash: u64,
    pub unit: [u8; UNIT_CAP],
    pub unit_len: u8,
    pub description: [u8; PARAM_DESC_CAP],
    pub description_len: u8,
    pub flags: u8,
    pub min: f64,
    pub max: f64,
    pub default_value: f64,
}

impl Default for ActionParam {
    fn default() -> Self {
        Self {
            name: [0; NAME_CAP],
            name_len: 0,
            name_hash: 0,
            unit: [0; UNIT_CAP],
            unit_len: 0,
            description: [0; PARAM_DESC_CAP],
            description_len: 0,
            flags: 0,
            min: 0.0,
            max: 0.0,
            default_value: 0.0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, ZeroCopySend)]
pub struct ActionAdvertise {
    pub from: u64,
    pub device: [u8; NAME_CAP],
    pub device_len: u8,
    pub device_hash: u64,
    pub action: [u8; NAME_CAP],
    pub action_len: u8,
    pub action_hash: u64,
    pub description: [u8; ACTION_DESC_CAP],
    pub description_len: u16,
    pub param_count: u8,
    pub params: [ActionParam; sim_kernel::MAX_ACTION_PARAMS],
}

impl Default for ActionAdvertise {
    fn default() -> Self {
        Self {
            from: 0,
            device: [0; NAME_CAP],
            device_len: 0,
            device_hash: 0,
            action: [0; NAME_CAP],
            action_len: 0,
            action_hash: 0,
            description: [0; ACTION_DESC_CAP],
            description_len: 0,
            param_count: 0,
            params: [ActionParam::default(); sim_kernel::MAX_ACTION_PARAMS],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, ZeroCopySend)]
pub struct RequestParam {
    pub name_hash: u64,
    pub value: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, ZeroCopySend)]
pub struct ActionRequest {
    pub request_id: u64,
    pub board_hash: u64,
    pub device_hash: u64,
    pub action_hash: u64,
    pub param_count: u8,
    pub params: [RequestParam; sim_kernel::MAX_ACTION_PARAMS],
}

impl Default for ActionRequest {
    fn default() -> Self {
        Self {
            request_id: 0,
            board_hash: 0,
            device_hash: 0,
            action_hash: 0,
            param_count: 0,
            params: [RequestParam {
                name_hash: 0,
                value: 0.0,
            }; sim_kernel::MAX_ACTION_PARAMS],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, ZeroCopySend)]
pub struct ActionResult {
    pub request_id: u64,
    pub status: u8,
}

/// Node → arbiter (and arbiter → server) sample.
///
/// The advertise variant is the catalog row; the result variant is two words.
/// Both stay inline so the sample remains `Copy` for `send_copy`.
#[allow(clippy::large_enum_variant)]
#[repr(C)]
#[derive(Clone, Copy, Debug, ZeroCopySend)]
pub enum ActionUpstream {
    Advertise(ActionAdvertise),
    Result(ActionResult),
}

#[derive(Debug)]
pub enum ActionPackError {
    TooLong { field: &'static str },
    HashMismatch { field: &'static str },
    TooManyParams,
}

impl std::fmt::Display for ActionPackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong { field } => write!(f, "{field} exceeds the action sample cap"),
            Self::HashMismatch { field } => write!(f, "{field} hash does not match its name"),
            Self::TooManyParams => write!(f, "too many action parameters"),
        }
    }
}

impl std::error::Error for ActionPackError {}

impl ActionAdvertise {
    /// Pack one catalog row into an advertise sample.
    #[must_use]
    pub fn new(from: u64, entry: &ActionCatalogEntry) -> Result<Self, ActionPackError> {
        if entry.device_hash != id_hash(&entry.device) {
            return Err(ActionPackError::HashMismatch { field: "device" });
        }
        if entry.action_hash != id_hash(&entry.action) {
            return Err(ActionPackError::HashMismatch { field: "action" });
        }
        if entry.params.len() > sim_kernel::MAX_ACTION_PARAMS {
            return Err(ActionPackError::TooManyParams);
        }
        let mut sample = ActionAdvertise {
            from,
            device_hash: entry.device_hash,
            action_hash: entry.action_hash,
            param_count: u8::try_from(entry.params.len()).unwrap_or(u8::MAX),
            ..ActionAdvertise::default()
        };
        sample.device_len = pack_str(&entry.device, &mut sample.device, "device")?;
        sample.action_len = pack_str(&entry.action, &mut sample.action, "action")?;
        sample.description_len = u16::from(pack_str(
            &entry.description,
            &mut sample.description,
            "description",
        )?);
        for (index, param) in entry.params.iter().enumerate() {
            sample.params[index] = ActionParam::new(param)?;
        }
        Ok(sample)
    }
}

impl ActionRequest {
    /// Pack an invoke into a request sample.
    #[must_use]
    pub fn new(
        request_id: u64,
        board_hash: u64,
        device_hash: u64,
        action_hash: u64,
        params: &[(u64, f64)],
    ) -> Result<Self, ActionPackError> {
        if params.len() > sim_kernel::MAX_ACTION_PARAMS {
            return Err(ActionPackError::TooManyParams);
        }
        let mut sample = ActionRequest {
            request_id,
            board_hash,
            device_hash,
            action_hash,
            param_count: u8::try_from(params.len()).unwrap_or(u8::MAX),
            ..ActionRequest::default()
        };
        for (index, (name_hash, value)) in params.iter().enumerate() {
            sample.params[index] = RequestParam {
                name_hash: *name_hash,
                value: *value,
            };
        }
        Ok(sample)
    }

    /// Parameter pairs carried on this request, truncated to `param_count`.
    #[must_use]
    pub fn raw_params(&self) -> Vec<(u64, f64)> {
        let n = usize::from(self.param_count).min(self.params.len());
        self.params[..n]
            .iter()
            .map(|param| (param.name_hash, param.value))
            .collect()
    }
}

impl AdvertisedAction {
    /// JSON catalog row. Hashes stay off this value.
    #[must_use]
    pub fn new(board_id: &str, sample: &ActionAdvertise) -> Result<Self, ActionPackError> {
        let device = unpack(&sample.device, sample.device_len);
        let action = unpack(&sample.action, sample.action_len);
        if sample.device_hash != id_hash(&device) {
            return Err(ActionPackError::HashMismatch { field: "device" });
        }
        if sample.action_hash != id_hash(&action) {
            return Err(ActionPackError::HashMismatch { field: "action" });
        }
        let n = usize::from(sample.param_count).min(sample.params.len());
        let mut params = Vec::with_capacity(n);
        for param in &sample.params[..n] {
            params.push(AdvertisedActionParam::new(param)?);
        }
        Ok(AdvertisedAction {
            board_id: board_id.to_string(),
            device,
            action,
            description: unpack(&sample.description, sample.description_len),
            params,
        })
    }
}

impl ActionParam {
    fn new(param: &ParamSpec) -> Result<Self, ActionPackError> {
        if param.name_hash != id_hash(&param.name) {
            return Err(ActionPackError::HashMismatch { field: "parameter" });
        }
        let mut packed = ActionParam {
            name_hash: param.name_hash,
            flags: 0,
            ..ActionParam::default()
        };
        packed.name_len = pack_str(&param.name, &mut packed.name, "parameter")?;
        if let Some(unit) = &param.unit {
            packed.unit_len = pack_str(unit, &mut packed.unit, "unit")?;
        }
        if let Some(description) = &param.description {
            packed.description_len = pack_str(
                description,
                &mut packed.description,
                "parameter description",
            )?;
        }
        if let Some(min) = param.min {
            packed.flags |= FLAG_MIN;
            packed.min = min;
        }
        if let Some(max) = param.max {
            packed.flags |= FLAG_MAX;
            packed.max = max;
        }
        if let Some(default_value) = param.default {
            packed.flags |= FLAG_DEFAULT;
            packed.default_value = default_value;
        }
        Ok(packed)
    }
}

impl AdvertisedActionParam {
    fn new(param: &ActionParam) -> Result<Self, ActionPackError> {
        let name = unpack(&param.name, param.name_len);
        if param.name_hash != id_hash(&name) {
            return Err(ActionPackError::HashMismatch { field: "parameter" });
        }
        let unit = optional_text(&param.unit, param.unit_len);
        let description = optional_text(&param.description, param.description_len);
        Ok(AdvertisedActionParam {
            name,
            unit,
            description,
            min: flag_f64(param.flags, FLAG_MIN, param.min),
            max: flag_f64(param.flags, FLAG_MAX, param.max),
            default: flag_f64(param.flags, FLAG_DEFAULT, param.default_value),
        })
    }
}

fn flag_f64(flags: u8, bit: u8, value: f64) -> Option<f64> {
    if flags & bit == 0 { None } else { Some(value) }
}

fn optional_text(buf: &[u8], len: u8) -> Option<String> {
    if len == 0 {
        None
    } else {
        Some(unpack(buf, len))
    }
}

fn pack_str(text: &str, buf: &mut [u8], field: &'static str) -> Result<u8, ActionPackError> {
    if text.len() > buf.len() {
        return Err(ActionPackError::TooLong { field });
    }
    buf[..text.len()].copy_from_slice(text.as_bytes());
    u8::try_from(text.len()).map_err(|_| ActionPackError::TooLong { field })
}

fn unpack(buf: &[u8], len: impl Into<usize>) -> String {
    let n = len.into().min(buf.len());
    match std::str::from_utf8(&buf[..n]) {
        Ok(text) => text.to_string(),
        Err(err) => String::from_utf8_lossy(&buf[..err.valid_up_to()]).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{ActionSpec, ParamSpec, id_hash};

    #[test]
    fn advertise_round_trip_drops_hashes_from_json_body() {
        let mut milliseconds = ParamSpec::new("milliseconds");
        milliseconds.unit = Some("ms".into());
        milliseconds.description = Some("押下を維持する仮想時間".into());
        milliseconds.min = Some(1.0);
        let mut spec = ActionSpec::new("hold", "指定した仮想時間だけスイッチを押す");
        spec.params.push(milliseconds);
        let entry = ActionCatalogEntry {
            device: "sw1".into(),
            device_hash: id_hash("sw1"),
            action: spec.id.clone(),
            action_hash: spec.hash,
            description: spec.description.clone(),
            params: spec.params.clone(),
        };
        let sample = ActionAdvertise::new(9, &entry).expect("pack");
        let body = AdvertisedAction::new("board-a", &sample).expect("unpack");
        assert_eq!(body.board_id, "board-a");
        assert_eq!(body.device, "sw1");
        assert_eq!(body.action, "hold");
        assert_eq!(body.params[0].name, "milliseconds");
        assert_eq!(body.params[0].unit.as_deref(), Some("ms"));
        assert_eq!(body.params[0].min, Some(1.0));
        assert!(body.params[0].max.is_none());
        assert!(body.params[0].default.is_none());
    }
}
