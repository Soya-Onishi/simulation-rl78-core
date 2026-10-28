//! Shared HTTP contract types for client↔web and web↔sim.
//!
//! These shapes are the stable wire vocabulary. Transports (axum handlers,
//! future HTTP clients to remote sim-servers) should reuse them.

use serde::{Deserialize, Serialize};

use crate::host::SessionNode;
use crate::topology::LogicalTopology;

/// Body for `POST /api/simulation`.
#[derive(Debug, Deserialize)]
pub struct CreateSimulationRequest {
    pub topology: LogicalTopology,
}

/// Body for `201` create responses.
#[derive(Debug, Serialize)]
pub struct CreateSimulationResponse {
    pub id: String,
}

/// SSE `status` event payload (and internal status snapshots).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusEvent {
    pub id: String,
    pub state: String,
    pub virtual_time_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub nodes: Vec<SessionNode>,
}

/// Body for `POST /api/simulation/{id}/start`. Empty / omitted `until_ns` is unlimited.
#[derive(Debug, Default, Deserialize)]
pub struct StartSimulationRequest {
    pub until_ns: Option<u64>,
}

impl StartSimulationRequest {
    /// Parse a start body. Empty bytes are unlimited.
    pub fn from_body(body: &[u8]) -> Result<Self, ApiError> {
        let trimmed = body.trim_ascii();
        if trimmed.is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_slice(trimmed)
            .map_err(|_| ApiError::bad_request("bad_param", "invalid start body"))
    }
}

/// SSE `log` event payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEvent {
    pub seq: u64,
    pub level: String,
    pub origin: String,
    pub board: Option<String>,
    pub text: String,
}

/// One parameter in an action catalog row. Hashes are not part of the JSON contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdvertisedActionParam {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<f64>,
}

/// One action a client can invoke. Identity fields are strings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdvertisedAction {
    pub board_id: String,
    pub device: String,
    pub action: String,
    pub description: String,
    pub params: Vec<AdvertisedActionParam>,
}

/// `GET /api/simulation/{id}/actions` body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionList {
    pub actions: Vec<AdvertisedAction>,
}

/// `POST /api/simulation/{id}/actions` body.
#[derive(Clone, Debug, Deserialize)]
pub struct InvokeActionRequest {
    pub board_id: String,
    pub device: String,
    pub action: String,
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, f64>,
}

/// Uniform API error envelope: `{"error":{"code","message"}}`.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

/// Failure returned through the HTTP layer and backend trait.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    #[must_use]
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(400, code, message)
    }

    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "not_found", message)
    }

    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, "conflict", message)
    }

    #[must_use]
    pub fn viewer_busy() -> Self {
        Self::new(409, "viewer_busy", "simulation is busy")
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "internal", message)
    }

    #[must_use]
    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            error: ErrorDetail {
                code: self.code.to_string(),
                message: self.message.clone(),
            },
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
mod tests {
    use super::StartSimulationRequest;

    #[test]
    fn start_body_empty_is_unlimited() {
        assert_eq!(
            StartSimulationRequest::from_body(b"").unwrap().until_ns,
            None
        );
        assert_eq!(
            StartSimulationRequest::from_body(b" \n\t")
                .unwrap()
                .until_ns,
            None
        );
        assert_eq!(
            StartSimulationRequest::from_body(b"{}").unwrap().until_ns,
            None
        );
    }

    #[test]
    fn start_body_reads_until_ns() {
        let parsed = StartSimulationRequest::from_body(b"{\"until_ns\":1000}").unwrap();
        assert_eq!(parsed.until_ns, Some(1000));
    }

    #[test]
    fn start_body_rejects_bad_json() {
        assert!(StartSimulationRequest::from_body(b"not-json").is_err());
        assert!(StartSimulationRequest::from_body(b"{\"until_ns\":\"x\"}").is_err());
    }
}
