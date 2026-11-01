//! JSON bodies used only on the internal HTTP API.

use serde::{Deserialize, Serialize};

use crate::probe_log::ProbePointName;

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthBody {
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExistsBody {
    pub exists: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HighWaterBody {
    pub seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewerTokenBody {
    pub token: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CsvStartBody {
    pub request_id: u64,
    pub probes: Vec<ProbePointName>,
}
