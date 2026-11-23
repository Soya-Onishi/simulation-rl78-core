//! Axum router for Orchestrator → Command REST.
//!
//! Routes are registered against [`contract`] `ROUTE` templates so a missing
//! path param fails at compile/review time rather than as an ambiguous contract.
//! Clients use the matching `url(...)` helpers.

use axum::extract::{Json, Path};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use contract::{
    build_ensure, build_restart, build_status, build_terminate, node_command, probe_interest,
    reset, sessions, start, status, stop, ClusterId, HashedId, NodeCommandBody, OrchestratorReply,
    ProbeInterestBody, RestErrorBody, RestErrorCode, StartBody,
};

type ApiError = (StatusCode, Json<RestErrorBody>);
type ApiResult = Result<(StatusCode, Json<OrchestratorReply>), ApiError>;

/// Build the Command HTTP API router (handlers return [`StatusCode::NOT_IMPLEMENTED`]).
pub fn command_router() -> Router {
    Router::new()
        .route(sessions::ROUTE, post(post_sessions))
        .route(start::ROUTE, post(post_start))
        .route(stop::ROUTE, post(post_stop))
        .route(reset::ROUTE, post(post_reset))
        .route(status::ROUTE, get(get_status))
        .route(node_command::ROUTE, post(post_node_command))
        .route(probe_interest::ROUTE, post(post_probe_interest))
        .route(build_ensure::ROUTE, post(post_build_ensure))
        .route(build_restart::ROUTE, post(post_build_restart))
        .route(build_terminate::ROUTE, post(post_build_terminate))
        .route(build_status::ROUTE, get(get_build_status))
}

fn not_implemented() -> ApiError {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(RestErrorBody {
            code: RestErrorCode::NotImplemented,
            message: "not implemented".into(),
        }),
    )
}

fn bad_path(message: impl Into<String>) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(RestErrorBody {
            code: RestErrorCode::BadRequest,
            message: message.into(),
        }),
    )
}

fn parse_cluster_id(raw: &str) -> Result<ClusterId, ApiError> {
    raw.parse()
        .map_err(|_| bad_path(format!("invalid cluster_id path segment: {raw}")))
}

fn parse_node_key(raw: &str) -> Result<HashedId<u64>, ApiError> {
    raw.parse()
        .map_err(|_| bad_path(format!("invalid node_key path segment: {raw}")))
}

fn parse_node(cluster_id: &str, node_key: &str) -> Result<(ClusterId, HashedId<u64>), ApiError> {
    Ok((parse_cluster_id(cluster_id)?, parse_node_key(node_key)?))
}

async fn post_sessions() -> ApiResult {
    Err(not_implemented())
}

async fn post_start(Path(cluster_id): Path<String>, Json(_body): Json<StartBody>) -> ApiResult {
    let _cluster_id = parse_cluster_id(&cluster_id)?;
    Err(not_implemented())
}

async fn post_stop(Path(cluster_id): Path<String>) -> ApiResult {
    let _cluster_id = parse_cluster_id(&cluster_id)?;
    Err(not_implemented())
}

async fn post_reset(Path(cluster_id): Path<String>) -> ApiResult {
    let _cluster_id = parse_cluster_id(&cluster_id)?;
    Err(not_implemented())
}

async fn get_status(Path(cluster_id): Path<String>) -> ApiResult {
    let _cluster_id = parse_cluster_id(&cluster_id)?;
    Err(not_implemented())
}

async fn post_node_command(
    Path((cluster_id, node_key)): Path<(String, String)>,
    Json(_body): Json<NodeCommandBody>,
) -> ApiResult {
    let (_cluster_id, _node_key) = parse_node(&cluster_id, &node_key)?;
    Err(not_implemented())
}

async fn post_probe_interest(
    Path((cluster_id, node_key)): Path<(String, String)>,
    Json(_body): Json<ProbeInterestBody>,
) -> ApiResult {
    let (_cluster_id, _node_key) = parse_node(&cluster_id, &node_key)?;
    Err(not_implemented())
}

async fn post_build_ensure(Path((cluster_id, node_key)): Path<(String, String)>) -> ApiResult {
    let (_cluster_id, _node_key) = parse_node(&cluster_id, &node_key)?;
    Err(not_implemented())
}

async fn post_build_restart(Path((cluster_id, node_key)): Path<(String, String)>) -> ApiResult {
    let (_cluster_id, _node_key) = parse_node(&cluster_id, &node_key)?;
    Err(not_implemented())
}

async fn post_build_terminate(Path((cluster_id, node_key)): Path<(String, String)>) -> ApiResult {
    let (_cluster_id, _node_key) = parse_node(&cluster_id, &node_key)?;
    Err(not_implemented())
}

/// `GET` [`build_status::ROUTE`] — ids are path params.
async fn get_build_status(Path((cluster_id, node_key)): Path<(String, String)>) -> ApiResult {
    let (_cluster_id, _node_key) = parse_node(&cluster_id, &node_key)?;
    Err(not_implemented())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn build_status_route_binds_path_ids() {
        let app = command_router();
        let cluster = ClusterId::new(1, 2);
        let node = "deadbeefcafebabe".parse::<HashedId<u64>>().unwrap();
        let url = build_status::url(cluster, node);
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(&url)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn build_status_rejects_bad_cluster_id() {
        let app = command_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/command/sessions/not-a-cluster/nodes/deadbeefcafebabe/build/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn build_status_rejects_bad_node_key() {
        let app = command_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/command/sessions/1-2/nodes/DEADBEEFCAFEBABE/build/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn build_status_without_ids_is_not_found() {
        let app = command_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/command/sessions/1-2/nodes/deadbeefcafebabe/build")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn start_route_uses_cluster_path() {
        let app = command_router();
        let url = start::url(ClusterId::new(7, 99));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&url)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
