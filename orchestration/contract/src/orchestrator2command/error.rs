//! Shared REST error body for Orchestrator ↔ Command (and later peers).

use serde::{Deserialize, Serialize};

/// Error body; HTTP status should be 4xx/5xx as noted on [`RestErrorCode`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestErrorBody {
    pub code: RestErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestErrorCode {
    /// HTTP 400
    BadRequest,
    /// HTTP 404
    NotFound,
    /// HTTP 409
    Conflict,
    /// HTTP 501
    NotImplemented,
    /// HTTP 503
    Unavailable,
    /// HTTP 500
    Internal,
}
