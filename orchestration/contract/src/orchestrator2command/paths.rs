//! Orchestrator → Command HTTP paths.
//!
//! Each endpoint is a submodule with:
//! - `ROUTE`: axum template (`{cluster_id}`, `{node_key}`)
//! - `url(...)`: concrete path for clients ([`ClusterId`] / [`HashedId`] [`Display`])
//!
//! Resource identity lives in the URI (same practice as `/api/simulation/{id}/…`).
//! `url` is always derived from `ROUTE` by filling placeholders, so the two cannot drift.

use crate::id::{ClusterId, HashedId};

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
            use super::ClusterId;

            pub const ROUTE: &str = command_path!("/sessions/{cluster_id}", $tail);

            #[must_use]
            pub fn url(cluster_id: ClusterId) -> String {
                ROUTE.replace("{cluster_id}", &cluster_id.to_string())
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
            use super::{ClusterId, HashedId};

            pub const ROUTE: &str =
                command_path!("/sessions/{cluster_id}/nodes/{node_key}", $tail);

            #[must_use]
            pub fn url(cluster_id: ClusterId, node_key: HashedId<u64>) -> String {
                ROUTE
                    .replace("{cluster_id}", &cluster_id.to_string())
                    .replace("{node_key}", &node_key.to_string())
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
    fn url_is_route_with_placeholders_filled() {
        let c = ClusterId::new(1, 2);
        let n = "deadbeefcafebabe".parse::<HashedId<u64>>().unwrap();

        assert_eq!(start::url(c), "/v1/command/sessions/1-2/start");
        assert_eq!(stop::url(c), "/v1/command/sessions/1-2/stop");
        assert_eq!(reset::url(c), "/v1/command/sessions/1-2/reset");
        assert_eq!(status::url(c), "/v1/command/sessions/1-2/status");
        assert_eq!(
            build_status::url(c, n),
            "/v1/command/sessions/1-2/nodes/deadbeefcafebabe/build/status"
        );
        assert_eq!(
            node_command::url(c, n),
            "/v1/command/sessions/1-2/nodes/deadbeefcafebabe/command"
        );
    }
}
