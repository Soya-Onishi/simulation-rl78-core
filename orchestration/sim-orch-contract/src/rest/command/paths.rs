//! Orchestrator → Command HTTP paths.
//!
//! Each endpoint is a submodule with:
//! - `ROUTE`: axum template (`{cluster_id}`, `{node_key}`)
//! - `url(...)`: concrete path for clients ([`ClusterId`] / [`HashedId`] [`Display`])
//!
//! Resource identity lives in the URI (same practice as `/api/simulation/{id}/…`).
//! `ROUTE` and `url` are generated from one literal tail so they cannot drift.

use crate::cluster_id::ClusterId;
use crate::hashed_id::HashedId;

/// Build a path under the Command API prefix (`/v1/command`).
macro_rules! command_path {
    () => {
        "/v1/command"
    };
    ($($part:literal),+ $(,)?) => {
        concat!("/v1/command", $($part),+)
    };
}

/// Mount / URL prefix for the Command host API (Orchestrator is the client).
pub const API_PREFIX: &str = command_path!();

/// `POST` — create a session; Command assigns [`ClusterId`].
pub mod sessions {
    pub const ROUTE: &str = command_path!("/sessions");

    #[must_use]
    pub fn url() -> &'static str {
        ROUTE
    }
}

/// Session-scoped route: `/sessions/{cluster_id}` + `$tail`.
macro_rules! session_route {
    ($name:ident, $tail:literal) => {
        pub mod $name {
            use super::{API_PREFIX, ClusterId};

            pub const ROUTE: &str = command_path!("/sessions/{cluster_id}", $tail);

            #[must_use]
            pub fn url(cluster_id: ClusterId) -> String {
                let mut path = format!("{API_PREFIX}/sessions/{cluster_id}");
                path.push_str($tail);
                path
            }
        }
    };
}

session_route!(start, "/start");
session_route!(stop, "/stop");
session_route!(reset, "/reset");
session_route!(status, "/status");

/// Node-scoped route: `/sessions/{cluster_id}/nodes/{node_key}` + `$tail`.
macro_rules! node_route {
    ($name:ident, $tail:literal) => {
        pub mod $name {
            use super::{API_PREFIX, ClusterId, HashedId};

            pub const ROUTE: &str =
                command_path!("/sessions/{cluster_id}/nodes/{node_key}", $tail);

            #[must_use]
            pub fn url(cluster_id: ClusterId, node_key: HashedId<u64>) -> String {
                let mut path = format!("{API_PREFIX}/sessions/{cluster_id}/nodes/{node_key}");
                path.push_str($tail);
                path
            }
        }
    };
}

node_route!(node_command, "/command");
node_route!(probe_interest, "/probe-interest");
node_route!(build_ensure, "/build/ensure");
node_route!(build_restart, "/build/restart");
node_route!(build_terminate, "/build/terminate");
node_route!(build_status, "/build/status");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_use_api_prefix_and_placeholders() {
        assert_eq!(sessions::ROUTE, "/v1/command/sessions");
        assert_eq!(start::ROUTE, "/v1/command/sessions/{cluster_id}/start");
        assert_eq!(
            node_command::ROUTE,
            "/v1/command/sessions/{cluster_id}/nodes/{node_key}/command"
        );
        assert_eq!(
            build_status::ROUTE,
            "/v1/command/sessions/{cluster_id}/nodes/{node_key}/build/status"
        );
    }

    #[test]
    fn url_matches_route_after_placeholder_fill() {
        let c = ClusterId::new(1, 2);
        let n = "deadbeefcafebabe".parse::<HashedId<u64>>().unwrap();

        let filled_session = start::ROUTE.replace("{cluster_id}", &c.to_string());
        assert_eq!(start::url(c), filled_session);

        let filled_node = build_status::ROUTE
            .replace("{cluster_id}", &c.to_string())
            .replace("{node_key}", &n.to_string());
        assert_eq!(build_status::url(c, n), filled_node);
    }
}
