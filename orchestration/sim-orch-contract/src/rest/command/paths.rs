//! Orchestrator → Command HTTP path templates.
//!
//! All routes hang under [`COMMAND_API_PREFIX`]. Axum registers the `PATH_*`
//! templates (including `{node_key}` where needed); clients use the `*_url`
//! helpers to substitute path params.
//!
//! `concat!` only accepts literals, so the prefix string is centralized in
//! [`command_path!`] rather than referenced as a `const` inside `concat!`.

/// Build a path under the Command API prefix (`/v1/command`).
macro_rules! command_path {
    ($suffix:literal) => {
        concat!("/v1/command", $suffix)
    };
}

/// Mount / URL prefix for the Command host API (Orchestrator is the client).
pub const COMMAND_API_PREFIX: &str = command_path!("");

/// `POST` — ensure / attach session.
pub const PATH_SESSION: &str = command_path!("/session");
/// `POST`
pub const PATH_START: &str = command_path!("/start");
/// `POST`
pub const PATH_STOP: &str = command_path!("/stop");
/// `POST`
pub const PATH_RESET: &str = command_path!("/reset");
/// `POST`
pub const PATH_NODE_COMMAND: &str = command_path!("/node");
/// `POST`
pub const PATH_PROBE_INTEREST: &str = command_path!("/probe-interest");
/// `GET`
pub const PATH_STATUS: &str = command_path!("/status");
/// `POST` — Build/Launch stub: spawn or ensure a node process.
pub const PATH_BUILD_ENSURE: &str = command_path!("/build/ensure");
/// `POST` — restart a node process.
pub const PATH_BUILD_RESTART: &str = command_path!("/build/restart");
/// `POST` — terminate a node process.
pub const PATH_BUILD_TERMINATE: &str = command_path!("/build/terminate");
/// `GET` — node process lifecycle status. Path param: `{node_key}`.
pub const PATH_BUILD_STATUS: &str = command_path!("/build/status/{node_key}");

/// Concrete URL for [`PATH_BUILD_STATUS`].
///
/// `node_key` is the opaque hex id (`HashedId`); it is not percent-encoded.
#[must_use]
pub fn build_status_url(node_key: crate::hashed_id::HashedId<u64>) -> String {
    format!("{COMMAND_API_PREFIX}/build/status/{node_key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_use_api_prefix() {
        for path in [
            PATH_SESSION,
            PATH_START,
            PATH_STOP,
            PATH_RESET,
            PATH_NODE_COMMAND,
            PATH_PROBE_INTEREST,
            PATH_STATUS,
            PATH_BUILD_ENSURE,
            PATH_BUILD_RESTART,
            PATH_BUILD_TERMINATE,
            PATH_BUILD_STATUS,
        ] {
            assert!(
                path.starts_with(COMMAND_API_PREFIX),
                "{path} must start with {COMMAND_API_PREFIX}"
            );
        }
    }

    #[test]
    fn build_status_template_and_url() {
        assert!(PATH_BUILD_STATUS.contains("{node_key}"));
        assert_eq!(
            build_status_url("deadbeefcafebabe".parse().unwrap()),
            "/v1/command/build/status/deadbeefcafebabe"
        );
    }
}
