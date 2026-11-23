//! Axum router for Orchestrator → Command REST.
//!
//! Routes are registered against [`sim_orch_contract`] path templates so a
//! missing path param (e.g. build status `node_key`) fails at compile/review
//! time rather than as an ambiguous contract.

use axum::extract::{Json, Path};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use sim_orch_contract::{
    OrchestratorReply, OrchestratorRequest, PATH_BUILD_ENSURE, PATH_BUILD_RESTART,
    PATH_BUILD_STATUS, PATH_BUILD_TERMINATE, PATH_NODE_COMMAND, PATH_PROBE_INTEREST, PATH_RESET,
    PATH_SESSION, PATH_START, PATH_STATUS, PATH_STOP, RestErrorBody, RestErrorCode,
};

/// Build the Command HTTP API router (handlers return [`StatusCode::NOT_IMPLEMENTED`]).
pub fn command_router() -> Router {
    Router::new()
        .route(PATH_SESSION, post(post_session))
        .route(PATH_START, post(post_start))
        .route(PATH_STOP, post(post_stop))
        .route(PATH_RESET, post(post_reset))
        .route(PATH_NODE_COMMAND, post(post_node_command))
        .route(PATH_PROBE_INTEREST, post(post_probe_interest))
        .route(PATH_STATUS, get(get_status))
        .route(PATH_BUILD_ENSURE, post(post_build_ensure))
        .route(PATH_BUILD_RESTART, post(post_build_restart))
        .route(PATH_BUILD_TERMINATE, post(post_build_terminate))
        .route(PATH_BUILD_STATUS, get(get_build_status))
}

fn not_implemented() -> (StatusCode, Json<RestErrorBody>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(RestErrorBody {
            code: RestErrorCode::NotImplemented,
            message: "not implemented".into(),
        }),
    )
}

async fn post_session(
    Json(_body): Json<OrchestratorRequest>,
) -> (StatusCode, Json<RestErrorBody>) {
    not_implemented()
}

async fn post_start(Json(_body): Json<OrchestratorRequest>) -> (StatusCode, Json<RestErrorBody>) {
    not_implemented()
}

async fn post_stop(Json(_body): Json<OrchestratorRequest>) -> (StatusCode, Json<RestErrorBody>) {
    not_implemented()
}

async fn post_reset(Json(_body): Json<OrchestratorRequest>) -> (StatusCode, Json<RestErrorBody>) {
    not_implemented()
}

async fn post_node_command(
    Json(_body): Json<OrchestratorRequest>,
) -> (StatusCode, Json<RestErrorBody>) {
    not_implemented()
}

async fn post_probe_interest(
    Json(_body): Json<OrchestratorRequest>,
) -> (StatusCode, Json<RestErrorBody>) {
    not_implemented()
}

async fn get_status() -> (StatusCode, Json<RestErrorBody>) {
    not_implemented()
}

async fn post_build_ensure(
    Json(_body): Json<OrchestratorRequest>,
) -> (StatusCode, Json<RestErrorBody>) {
    not_implemented()
}

async fn post_build_restart(
    Json(_body): Json<OrchestratorRequest>,
) -> (StatusCode, Json<RestErrorBody>) {
    not_implemented()
}

async fn post_build_terminate(
    Json(_body): Json<OrchestratorRequest>,
) -> (StatusCode, Json<RestErrorBody>) {
    not_implemented()
}

/// `GET` [`PATH_BUILD_STATUS`] — `node_key` is selected by the path param.
async fn get_build_status(
    Path(node_key): Path<String>,
) -> Result<(StatusCode, Json<OrchestratorReply>), (StatusCode, Json<RestErrorBody>)> {
    let _ = node_key;
    Err(not_implemented())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sim_orch_contract::build_status_url;
    use tower::ServiceExt;

    #[tokio::test]
    async fn build_status_route_binds_node_key() {
        let app = command_router();
        let url = build_status_url("deadbeefcafebabe");
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
    async fn build_status_without_key_is_not_found() {
        let app = command_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/command/build/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
