//! Internal HTTP adapter: HTTP → [`SimulationBackend`].

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::contract::{
    ActionList, ApiError, CreateSimulationRequest, CreateSimulationResponse, HashedIdString,
    InvokeActionRequest, LogEvent, ProbeSamplesResponse, StartSimulationRequest, StatusEvent,
    TopologyView,
};
use crate::inspect::{InspectReply, InspectRequest};
use crate::ipc::HashedId;
use crate::probe_log::{
    CoarseBatch, CsvPoll, ProbeCatalogItem, ProbeRange, ProbeRef, ProbeWatchView,
};

use super::SimulationBackend;
use super::wire::{CsvStartBody, ExistsBody, HealthBody, HighWaterBody, ViewerTokenBody};

const MAX_BODY: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    backend: Arc<dyn SimulationBackend>,
}

/// Serve the internal `/internal` API on `listen` (`host:port`).
pub async fn serve_internal(
    listen: &str,
    backend: Arc<dyn SimulationBackend>,
) -> Result<(), std::io::Error> {
    let app = router(backend);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;
    eprintln!("cluster-backend listening on http://{addr}");
    axum::serve(listener, app).await
}

pub fn router(backend: Arc<dyn SimulationBackend>) -> Router {
    let state = AppState { backend };
    Router::new()
        .route("/internal/health", get(health))
        .route("/internal/simulations", post(create))
        .route("/internal/simulations/{id}/start", post(start))
        .route("/internal/simulations/{id}/stop", post(stop))
        .route("/internal/simulations/{id}/reset", post(reset))
        .route("/internal/simulations/{id}", axum::routing::delete(delete))
        .route("/internal/simulations/{id}/exists", get(exists))
        .route("/internal/simulations/{id}/status", get(status))
        .route("/internal/simulations/{id}/topology", get(topology))
        .route("/internal/simulations/{id}/logs", get(logs_after))
        .route(
            "/internal/simulations/{id}/logs/high-water",
            get(log_high_water),
        )
        .route(
            "/internal/simulations/{id}/actions",
            get(actions).post(invoke_action),
        )
        .route(
            "/internal/simulations/{id}/probes/catalog",
            get(probe_catalog),
        )
        .route(
            "/internal/simulations/{id}/probes/watch",
            get(probe_watch_get)
                .put(probe_watch_put)
                .delete(probe_watch_clear),
        )
        .route(
            "/internal/simulations/{id}/probes/live",
            get(probe_watch_live),
        )
        .route(
            "/internal/simulations/{id}/probes/csv/start",
            post(csv_start),
        )
        .route(
            "/internal/simulations/{id}/probes/csv/{request_id}/poll",
            get(csv_poll),
        )
        .route(
            "/internal/simulations/{id}/probes/csv/{request_id}/status",
            get(csv_status),
        )
        .route(
            "/internal/simulations/{id}/probes/samples",
            get(probe_samples),
        )
        .route(
            "/internal/simulations/{id}/viewer/attach",
            post(viewer_attach),
        )
        .route(
            "/internal/simulations/{id}/viewer/check",
            post(viewer_check),
        )
        .route(
            "/internal/simulations/{id}/viewer/detach",
            post(viewer_detach),
        )
        .route(
            "/internal/simulations/{id}/boards/{board_id}/inspect",
            post(inspect),
        )
        .route(
            "/internal/simulations/{id}/boards/{board_id}/step",
            post(step),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?
}

async fn health() -> Json<HealthBody> {
    Json(HealthBody {
        status: "ok".into(),
    })
}

async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateSimulationRequest>,
) -> Result<(StatusCode, Json<CreateSimulationResponse>), ApiError> {
    let backend = state.backend.clone();
    let id = blocking(move || backend.create(body.topology)).await?;
    Ok((StatusCode::CREATED, Json(CreateSimulationResponse { id })))
}

async fn start(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<StartSimulationRequest>>,
) -> Result<StatusCode, ApiError> {
    let until_ns = body.map(|Json(req)| req.until_ns).unwrap_or(None);
    let backend = state.backend.clone();
    blocking(move || backend.start(&id, until_ns)).await?;
    Ok(StatusCode::OK)
}

async fn stop(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let backend = state.backend.clone();
    blocking(move || backend.stop(&id)).await?;
    Ok(StatusCode::OK)
}

async fn reset(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let backend = state.backend.clone();
    blocking(move || backend.reset(&id)).await?;
    Ok(StatusCode::OK)
}

async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let backend = state.backend.clone();
    blocking(move || backend.shutdown(&id)).await?;
    Ok(StatusCode::OK)
}

async fn exists(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ExistsBody>, ApiError> {
    let backend = state.backend.clone();
    let exists = blocking(move || backend.exists(&id)).await?;
    Ok(Json(ExistsBody { exists }))
}

async fn status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StatusEvent>, ApiError> {
    let backend = state.backend.clone();
    Ok(Json(blocking(move || backend.status(&id)).await?))
}

async fn topology(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TopologyView>, ApiError> {
    let backend = state.backend.clone();
    Ok(Json(blocking(move || backend.topology(&id)).await?))
}

#[derive(serde::Deserialize)]
struct AfterQuery {
    after: u64,
}

async fn logs_after(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<AfterQuery>,
) -> Result<Json<Vec<LogEvent>>, ApiError> {
    let backend = state.backend.clone();
    Ok(Json(
        blocking(move || backend.logs_after(&id, query.after)).await?,
    ))
}

async fn log_high_water(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<HighWaterBody>, ApiError> {
    let backend = state.backend.clone();
    let seq = blocking(move || backend.log_high_water(&id)).await?;
    Ok(Json(HighWaterBody { seq }))
}

async fn actions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ActionList>, ApiError> {
    let backend = state.backend.clone();
    Ok(Json(blocking(move || backend.actions(&id)).await?))
}

async fn invoke_action(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<InvokeActionRequest>,
) -> Result<StatusCode, ApiError> {
    let backend = state.backend.clone();
    blocking(move || backend.invoke_action(&id, request)).await?;
    Ok(StatusCode::OK)
}

async fn probe_catalog(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ProbeCatalogItem>>, ApiError> {
    let backend = state.backend.clone();
    Ok(Json(blocking(move || backend.probe_catalog(&id)).await?))
}

async fn probe_watch_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ProbeWatchView>, ApiError> {
    let backend = state.backend.clone();
    Ok(Json(blocking(move || backend.probe_watch_get(&id)).await?))
}

async fn probe_watch_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(probes): Json<Vec<ProbeRef>>,
) -> Result<Json<ProbeWatchView>, ApiError> {
    let backend = state.backend.clone();
    Ok(Json(
        blocking(move || backend.probe_watch_put(&id, probes)).await?,
    ))
}

async fn probe_watch_clear(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let backend = state.backend.clone();
    blocking(move || backend.probe_watch_clear(&id)).await?;
    Ok(StatusCode::OK)
}

async fn probe_watch_live(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<AfterQuery>,
) -> Result<Json<Vec<CoarseBatch>>, ApiError> {
    let backend = state.backend.clone();
    Ok(Json(
        blocking(move || backend.probe_watch_live(&id, query.after)).await?,
    ))
}

async fn csv_start(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(range): Json<ProbeRange>,
) -> Result<Json<CsvStartBody>, ApiError> {
    let backend = state.backend.clone();
    let (request_id, probes) = blocking(move || backend.csv_start(&id, range)).await?;
    Ok(Json(CsvStartBody { request_id, probes }))
}

#[derive(serde::Deserialize)]
struct CsvPath {
    id: String,
    request_id: u64,
}

async fn csv_poll(
    State(state): State<AppState>,
    Path(path): Path<CsvPath>,
) -> Result<Json<CsvPoll>, ApiError> {
    let backend = state.backend.clone();
    Ok(Json(
        blocking(move || backend.csv_poll(&path.id, path.request_id)).await?,
    ))
}

async fn csv_status(
    State(state): State<AppState>,
    Path(path): Path<CsvPath>,
) -> Result<Json<CsvPoll>, ApiError> {
    let backend = state.backend.clone();
    Ok(Json(
        blocking(move || backend.csv_status(&path.id, path.request_id)).await?,
    ))
}

#[derive(serde::Deserialize)]
struct SamplesQuery {
    board: HashedIdString,
    probe: HashedIdString,
    from_ns: Option<u64>,
    to_ns: Option<u64>,
}

async fn probe_samples(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<SamplesQuery>,
) -> Result<Json<ProbeSamplesResponse>, ApiError> {
    if query.probe.is_empty() {
        return Err(ApiError::bad_request("bad_param", "probe is required"));
    }
    let board = query.board.parse_id("board")?;
    let probe = query.probe.parse_id("probe")?;
    let backend = state.backend.clone();
    Ok(Json(
        blocking(move || backend.probe_samples(&id, board, probe, query.from_ns, query.to_ns))
            .await?,
    ))
}

async fn viewer_attach(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ViewerTokenBody>, ApiError> {
    let backend = state.backend.clone();
    let token = blocking(move || backend.viewer_attach(&id)).await?;
    Ok(Json(ViewerTokenBody { token }))
}

async fn viewer_check(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ViewerTokenBody>,
) -> Result<StatusCode, ApiError> {
    let backend = state.backend.clone();
    blocking(move || backend.viewer_check(&id, &body.token)).await?;
    Ok(StatusCode::OK)
}

async fn viewer_detach(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ViewerTokenBody>,
) -> Result<StatusCode, ApiError> {
    let backend = state.backend.clone();
    blocking(move || backend.viewer_detach(&id, &body.token)).await?;
    Ok(StatusCode::OK)
}

#[derive(serde::Deserialize)]
struct BoardPath {
    id: String,
    board_id: HashedId<u64>,
}

async fn inspect(
    State(state): State<AppState>,
    Path(path): Path<BoardPath>,
    Json(req): Json<InspectRequest>,
) -> Result<Json<InspectReply>, ApiError> {
    let backend = state.backend.clone();
    Ok(Json(
        blocking(move || backend.inspect(&path.id, path.board_id, req)).await?,
    ))
}

async fn step(
    State(state): State<AppState>,
    Path(path): Path<BoardPath>,
) -> Result<StatusCode, ApiError> {
    let backend = state.backend.clone();
    blocking(move || backend.step(&path.id, path.board_id)).await?;
    Ok(StatusCode::OK)
}
