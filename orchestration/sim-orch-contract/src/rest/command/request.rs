//! Orchestrator → Command request bodies.
//!
//! Resource ids (`cluster_id`, `node_key`) are path params — see [`super::paths`].

use serde::{Deserialize, Serialize};

use crate::hashed_id::HashedId;
use crate::rest::node_dto::NodeCommandDto;

/// `POST` [`super::paths::start`] body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartBody {
    /// `None` / omitted = run without a virtual-time ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ns: Option<u64>,
}

/// `POST` [`super::paths::node_command`] body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeCommandBody {
    pub command: NodeCommandDto,
}

/// `POST` [`super::paths::probe_interest`] body — **add** probe ids for aggregation
/// toward Orchestrator (same add semantics as wire `NodeCommand::ProbeSubscribe`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeInterestBody {
    pub probe_ids: Vec<HashedId<u64>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_body_omits_none_until_ns() {
        let body = StartBody { until_ns: None };
        assert_eq!(serde_json::to_value(&body).unwrap(), serde_json::json!({}));
        let back: StartBody = serde_json::from_str("{}").unwrap();
        assert_eq!(back.until_ns, None);
    }

    #[test]
    fn probe_interest_round_trip() {
        let body = ProbeInterestBody {
            probe_ids: vec![HashedId::new(1), HashedId::new(2)],
        };
        let s = serde_json::to_string(&body).unwrap();
        let back: ProbeInterestBody = serde_json::from_str(&s).unwrap();
        assert_eq!(back, body);
    }
}
