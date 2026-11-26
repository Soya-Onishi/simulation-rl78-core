//! Axum router for Orchestrator → Command REST.
//!
//! Routes are registered against [`contract`] `ROUTE` templates so a missing
//! path param fails at compile/review time rather than as an ambiguous contract.
//! Clients use the matching `url(...)` helpers. Handlers parse the request and
//! forward it to the Command thread; they hold no state of their own.

use std::sync::mpsc;

use axum::Router;
use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use contract::{
    ClusterId, HashedId, NodeCommand, NodeCommandBody, OrchestratorReply, ProbeInterestBody,
    RestErrorBody, RestErrorCode, SessionsBody, StartBody, build_ensure, build_restart,
    build_status, build_terminate, node_command, probe_interest, reset, sessions, start, status,
    stop,
};
use tokio::sync::oneshot;

use crate::command::{CommandError, CommandOp, CommandRequest, NodeOp, NodeRef, SessionOp};

type ApiError = (StatusCode, Json<RestErrorBody>);
type ApiResult = Result<(StatusCode, Json<OrchestratorReply>), ApiError>;

/// Router state: the Command thread's request queue.
#[derive(Clone)]
pub(crate) struct CommandHandle(mpsc::Sender<CommandRequest>);

impl CommandHandle {
    pub(crate) fn new(requests: mpsc::Sender<CommandRequest>) -> Self {
        Self(requests)
    }

    async fn dispatch(&self, op: CommandOp) -> ApiResult {
        let stopped = || {
            api_error(&CommandError::new(
                RestErrorCode::Unavailable,
                "command thread stopped",
            ))
        };
        let (reply, reply_rx) = oneshot::channel();
        self.0
            .send(CommandRequest { op, reply })
            .map_err(|_| stopped())?;
        match reply_rx.await.map_err(|_| stopped())? {
            Ok(reply) => Ok((StatusCode::OK, Json(reply))),
            Err(e) => Err(api_error(&e)),
        }
    }
}

pub(crate) fn command_router(handle: CommandHandle) -> Router {
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
        .with_state(handle)
}

fn status_code(code: RestErrorCode) -> StatusCode {
    match code {
        RestErrorCode::BadRequest => StatusCode::BAD_REQUEST,
        RestErrorCode::NotFound => StatusCode::NOT_FOUND,
        RestErrorCode::Conflict => StatusCode::CONFLICT,
        RestErrorCode::NotImplemented => StatusCode::NOT_IMPLEMENTED,
        RestErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        RestErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn api_error(e: &CommandError) -> ApiError {
    (
        status_code(e.code),
        Json(RestErrorBody {
            code: e.code,
            message: e.message.clone(),
        }),
    )
}

fn bad_request(message: impl Into<String>) -> ApiError {
    api_error(&CommandError::new(RestErrorCode::BadRequest, message))
}

fn parse_cluster_id(raw: &str) -> Result<ClusterId, ApiError> {
    raw.parse()
        .map_err(|_| bad_request(format!("invalid cluster_id path segment: {raw}")))
}

fn parse_node(cluster_id: &str, node_key: &str) -> Result<NodeRef, ApiError> {
    Ok(NodeRef {
        cluster: parse_cluster_id(cluster_id)?,
        node_key: node_key
            .parse::<HashedId<u64>>()
            .map_err(|_| bad_request(format!("invalid node_key path segment: {node_key}")))?,
    })
}

async fn post_sessions(
    State(handle): State<CommandHandle>,
    Json(body): Json<SessionsBody>,
) -> ApiResult {
    let (_, reply) = handle.dispatch(CommandOp::CreateSession(body)).await?;
    Ok((StatusCode::CREATED, reply))
}

async fn session_op(handle: &CommandHandle, cluster_id: &str, op: SessionOp) -> ApiResult {
    let cluster = parse_cluster_id(cluster_id)?;
    handle.dispatch(CommandOp::Session(cluster, op)).await
}

async fn node_op(
    handle: &CommandHandle,
    (cluster_id, node_key): (String, String),
    op: NodeOp,
) -> ApiResult {
    let node = parse_node(&cluster_id, &node_key)?;
    handle.dispatch(CommandOp::Node(node, op)).await
}

async fn post_start(
    State(handle): State<CommandHandle>,
    Path(cluster_id): Path<String>,
    Json(body): Json<StartBody>,
) -> ApiResult {
    if body.until_ns.is_some() {
        return Err(bad_request("until_ns is not supported until Phase 3"));
    }
    session_op(&handle, &cluster_id, SessionOp::Start).await
}

async fn post_stop(
    State(handle): State<CommandHandle>,
    Path(cluster_id): Path<String>,
) -> ApiResult {
    session_op(&handle, &cluster_id, SessionOp::Stop).await
}

async fn post_reset(
    State(handle): State<CommandHandle>,
    Path(cluster_id): Path<String>,
) -> ApiResult {
    session_op(&handle, &cluster_id, SessionOp::Reset).await
}

async fn get_status(
    State(handle): State<CommandHandle>,
    Path(cluster_id): Path<String>,
) -> ApiResult {
    session_op(&handle, &cluster_id, SessionOp::Status).await
}

async fn post_node_command(
    State(handle): State<CommandHandle>,
    Path(ids): Path<(String, String)>,
    Json(body): Json<NodeCommandBody>,
) -> ApiResult {
    let command = body
        .command
        .to_wire()
        .map_err(|e| bad_request(format!("node command: {e:?}")))?;
    node_op(&handle, ids, NodeOp::Command(command)).await
}

async fn post_probe_interest(
    State(handle): State<CommandHandle>,
    Path(ids): Path<(String, String)>,
    Json(body): Json<ProbeInterestBody>,
) -> ApiResult {
    let command = NodeCommand::probe_subscribe(&body.probe_ids)
        .ok_or_else(|| bad_request("probe_interest: too many probe ids"))?;
    node_op(&handle, ids, NodeOp::Command(command)).await
}

async fn post_build_ensure(
    State(handle): State<CommandHandle>,
    Path(ids): Path<(String, String)>,
) -> ApiResult {
    node_op(&handle, ids, NodeOp::BuildEnsure).await
}

async fn post_build_restart(
    State(handle): State<CommandHandle>,
    Path(ids): Path<(String, String)>,
) -> ApiResult {
    node_op(&handle, ids, NodeOp::BuildRestart).await
}

async fn post_build_terminate(
    State(handle): State<CommandHandle>,
    Path(ids): Path<(String, String)>,
) -> ApiResult {
    node_op(&handle, ids, NodeOp::BuildTerminate).await
}

/// `GET` [`build_status::ROUTE`] — ids are path params.
async fn get_build_status(
    State(handle): State<CommandHandle>,
    Path(ids): Path<(String, String)>,
) -> ApiResult {
    node_op(&handle, ids, NodeOp::BuildStatus).await
}
