//! Axum HTTP surface for the cluster control plane and embedded UI.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};

use crate::backend::{SimulationBackend, prepare_topology};
use crate::contract::{
    ActionList, ApiError, CreateSimulationRequest, CreateSimulationResponse, InvokeActionRequest,
    StartSimulationRequest,
};
use crate::probe_log::{self, ProbeRange, ProbeRef};
use crate::web::PAGE_HTML;

/// For ELF Size, max body size must be changed.
const MAX_BODY: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn SimulationBackend>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.body())).into_response()
    }
}

/// Build the application router (API under `/api`, UI at `/` and `/{id}`).
pub fn router(backend: Arc<dyn SimulationBackend>) -> Router {
    let state = AppState { backend };
    Router::new()
        .route("/", get(create_page))
        .route("/api/simulation", post(create_simulation))
        .route("/api/simulation/{id}/start", post(start_simulation))
        .route("/api/simulation/{id}/stop", post(stop_simulation))
        .route("/api/simulation/{id}/reset", post(reset_simulation))
        .route(
            "/api/simulation/{id}/actions",
            get(list_actions).post(invoke_action),
        )
        .route("/api/simulation/{id}", delete(delete_simulation))
        .route("/api/simulation/{id}/status", get(status_sse))
        .route("/api/simulation/{id}/viewer", get(viewer_sse))
        .route("/api/simulation/{id}/log", get(log_sse))
        .route("/api/simulation/{id}/probes/catalog", get(probe_catalog))
        .route(
            "/api/simulation/{id}/probes/watch",
            get(probe_watch_get)
                .put(probe_watch_put)
                .delete(probe_watch_clear),
        )
        .route("/api/simulation/{id}/probes/stream", get(probe_watch_sse))
        .route("/api/simulation/{id}/probes.csv", get(probes_csv))
        .route(
            "/api/simulation/{id}/probes/csv-status",
            get(probes_csv_status),
        )
        .route("/{id}", get(simulation_page))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

async fn create_page() -> Html<&'static str> {
    Html(PAGE_HTML)
}

async fn simulation_page(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match ensure_exists(&state, &id).await {
        Ok(()) => Html(PAGE_HTML).into_response(),
        Err(_) => Redirect::to("/").into_response(),
    }
}

async fn create_simulation(
    State(state): State<AppState>,
    Json(body): Json<CreateSimulationRequest>,
) -> Result<(StatusCode, Json<CreateSimulationResponse>), ApiError> {
    let base = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let topology = prepare_topology(body.topology, &base)?;
    let backend = state.backend.clone();
    let id = tokio::task::spawn_blocking(move || backend.create(topology))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok((StatusCode::CREATED, Json(CreateSimulationResponse { id })))
}

#[derive(Default, serde::Deserialize)]
struct ViewerParam {
    viewer: Option<String>,
}

fn token_from(headers: &HeaderMap, query: &ViewerParam) -> Option<String> {
    if let Some(token) = query.viewer.as_deref().filter(|text| !text.is_empty()) {
        return Some(token.to_string());
    }
    headers
        .get("x-viewer-token")
        .and_then(|value| value.to_str().ok())
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

async fn require_viewer(
    state: &AppState,
    id: &str,
    headers: &HeaderMap,
    query: &ViewerParam,
) -> Result<(), ApiError> {
    let Some(token) = token_from(headers, query) else {
        return Err(ApiError::viewer_busy());
    };
    let backend = state.backend.clone();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || backend.viewer_check(&id, &token))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(())
}

async fn start_simulation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    let until_ns = StartSimulationRequest::from_body(&body)?.until_ns;
    run_command(state, id, move |b, id| b.start(id, until_ns)).await
}

async fn stop_simulation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
) -> Result<StatusCode, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    run_command(state, id, |b, id| b.stop(id)).await
}

async fn reset_simulation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
) -> Result<StatusCode, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    run_command(state, id, |b, id| b.reset(id)).await
}

async fn list_actions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
) -> Result<Json<ActionList>, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    let backend = state.backend.clone();
    let list = tokio::task::spawn_blocking(move || backend.actions(&id))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(Json(list))
}

async fn invoke_action(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
    body: Result<Json<InvokeActionRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    let Json(request) =
        body.map_err(|_| ApiError::bad_request("bad_param", "invalid action parameter"))?;
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || backend.invoke_action(&id, request))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(StatusCode::ACCEPTED)
}

async fn run_command<F>(state: AppState, id: String, f: F) -> Result<StatusCode, ApiError>
where
    F: FnOnce(Arc<dyn SimulationBackend>, &str) -> Result<(), ApiError> + Send + 'static,
{
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || f(backend, &id))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(StatusCode::ACCEPTED)
}

async fn delete_simulation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
) -> Result<StatusCode, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || backend.delete(&id))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(StatusCode::OK)
}

struct ViewerGuard {
    backend: Arc<dyn SimulationBackend>,
    id: String,
    token: String,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        let _ = self.backend.viewer_detach(&self.id, &self.token);
    }
}

async fn viewer_sse(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    ensure_exists(&state, &id).await?;
    let backend = state.backend.clone();
    let id_attach = id.clone();
    let token = tokio::task::spawn_blocking(move || backend.viewer_attach(&id_attach))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    let guard = ViewerGuard {
        backend: state.backend.clone(),
        id,
        token: token.clone(),
    };
    let stream = stream::unfold((true, token, guard), |(first, token, guard)| async move {
        if !first {
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
        Some((
            Ok(Event::default().event("viewer").data(token.clone())),
            (false, token, guard),
        ))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn status_sse(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    let stream = status_event_stream(state.backend.clone(), id);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn log_sse(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    let backend = state.backend.clone();
    let id_hw = id.clone();
    let after = tokio::task::spawn_blocking(move || backend.log_high_water(&id_hw))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    let stream = log_event_stream(state.backend.clone(), id, after);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn status_event_stream(
    backend: Arc<dyn SimulationBackend>,
    id: String,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold(
        (backend, id, None::<crate::contract::StatusEvent>),
        |(backend, id, last)| async move {
            loop {
                let backend_poll = backend.clone();
                let id_poll = id.clone();
                let result =
                    tokio::task::spawn_blocking(move || backend_poll.status(&id_poll)).await;
                match result {
                    Ok(Ok(status)) => {
                        if last.as_ref() != Some(&status) {
                            let data =
                                serde_json::to_string(&status).unwrap_or_else(|_| "{}".into());
                            let event = Event::default().event("status").data(data);
                            return Some((Ok(event), (backend, id, Some(status))));
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Ok(Err(err)) if err.status == 404 => return None,
                    Ok(Err(_)) | Err(_) => {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        },
    )
}

fn log_event_stream(
    backend: Arc<dyn SimulationBackend>,
    id: String,
    after: u64,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold(
        (backend, id, after, Vec::<Event>::new()),
        |(backend, id, mut after, mut pending)| async move {
            loop {
                if let Some(event) = pending.pop() {
                    return Some((Ok(event), (backend, id, after, pending)));
                }

                let backend_poll = backend.clone();
                let id_poll = id.clone();
                let result =
                    tokio::task::spawn_blocking(move || backend_poll.logs_after(&id_poll, after))
                        .await;
                match result {
                    Ok(Ok(logs)) if !logs.is_empty() => {
                        let mut batch = Vec::with_capacity(logs.len());
                        for log in logs {
                            after = after.max(log.seq);
                            let data = serde_json::to_string(&log).unwrap_or_else(|_| "{}".into());
                            batch.push(Event::default().event("log").data(data));
                        }
                        // FIFO via pop from reversed buffer.
                        batch.reverse();
                        pending = batch;
                    }
                    Ok(Ok(_)) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Ok(Err(err)) if err.status == 404 => return None,
                    Ok(Err(_)) | Err(_) => {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        },
    )
}

#[derive(serde::Deserialize)]
struct ProbeCsvQuery {
    format: Option<String>,
    from_ns: Option<String>,
    to_ns: Option<String>,
    viewer: Option<String>,
}

async fn probe_catalog(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
) -> Result<Json<Vec<crate::probe_log::ProbeCatalogItem>>, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    let backend = state.backend.clone();
    let items = tokio::task::spawn_blocking(move || backend.probe_catalog(&id))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(Json(items))
}

#[derive(serde::Deserialize)]
struct WatchPutBody {
    probes: Vec<ProbeRef>,
}

async fn probe_watch_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
) -> Result<Json<crate::probe_log::ProbeWatchView>, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    let backend = state.backend.clone();
    let view = tokio::task::spawn_blocking(move || backend.probe_watch_get(&id))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(Json(view))
}

async fn probe_watch_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
    Json(body): Json<WatchPutBody>,
) -> Result<Json<crate::probe_log::ProbeWatchView>, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    let backend = state.backend.clone();
    let view = tokio::task::spawn_blocking(move || backend.probe_watch_put(&id, body.probes))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(Json(view))
}

async fn probe_watch_clear(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
) -> Result<StatusCode, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || backend.probe_watch_clear(&id))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(StatusCode::OK)
}

async fn probe_watch_sse(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewerParam>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    require_viewer(&state, &id, &headers, &query).await?;
    let backend = state.backend.clone();
    let id_live = id.clone();
    let first = tokio::task::spawn_blocking(move || backend.probe_watch_live(&id_live, 0))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    let after = first.iter().map(|batch| batch.seq).max().unwrap_or(0);
    let stream = probe_event_stream(state.backend.clone(), id, first, after);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn probe_event_stream(
    backend: Arc<dyn SimulationBackend>,
    id: String,
    first: Vec<crate::probe_log::CoarseBatch>,
    after: u64,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let mut pending = Vec::with_capacity(first.len());
    for batch in first.into_iter().rev() {
        let data = serde_json::to_string(&batch).unwrap_or_else(|_| "{}".into());
        pending.push(Event::default().event("probe").data(data));
    }
    stream::unfold(
        (backend, id, after, pending),
        |(backend, id, mut after, mut pending)| async move {
            loop {
                if let Some(event) = pending.pop() {
                    return Some((Ok(event), (backend, id, after, pending)));
                }
                let backend_poll = backend.clone();
                let id_poll = id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    backend_poll.probe_watch_live(&id_poll, after)
                })
                .await;
                match result {
                    Ok(Ok(batches)) if !batches.is_empty() => {
                        let mut events = Vec::with_capacity(batches.len());
                        for batch in batches {
                            after = after.max(batch.seq);
                            let data =
                                serde_json::to_string(&batch).unwrap_or_else(|_| "{}".into());
                            events.push(Event::default().event("probe").data(data));
                        }
                        events.reverse();
                        pending = events;
                    }
                    Ok(Ok(_)) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Ok(Err(err)) if err.status == 404 => return None,
                    Ok(Err(_)) | Err(_) => {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        },
    )
}

async fn probes_csv(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ProbeCsvQuery>,
) -> Result<Response, ApiError> {
    if let Some(format) = query.format.as_deref()
        && format != "csv"
    {
        return Err(ApiError::bad_request("bad_param", "invalid probe format"));
    }
    let viewer = ViewerParam {
        viewer: query.viewer.clone(),
    };
    require_viewer(&state, &id, &headers, &viewer).await?;
    let range = ProbeRange::parse(query.from_ns.as_deref(), query.to_ns.as_deref())
        .map_err(|message| ApiError::bad_request("bad_param", message))?;
    let backend = state.backend.clone();
    let id_start = id.clone();
    let (request_id, catalog) = tokio::task::spawn_blocking({
        let backend = backend.clone();
        move || backend.csv_start(&id_start, range)
    })
    .await
    .map_err(|err| ApiError::internal(err.to_string()))??;
    let id_first = id.clone();
    let first = tokio::task::spawn_blocking({
        let backend = backend.clone();
        move || backend.csv_poll(&id_first, request_id)
    })
    .await
    .map_err(|err| ApiError::internal(err.to_string()))??;
    if first.done && first.failed && first.records.is_empty() {
        return Err(ApiError::new(
            504,
            "timeout",
            "probe csv was not answered before the deadline",
        ));
    }
    let stream = csv_event_stream(backend, id, request_id, catalog, first, range);
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"probes.csv\""),
    );
    if let Ok(value) = HeaderValue::from_str(&request_id.to_string()) {
        response.headers_mut().insert("x-probe-csv-id", value);
    }
    Ok(response)
}

fn csv_event_stream(
    backend: Arc<dyn SimulationBackend>,
    id: String,
    request_id: u64,
    catalog: Vec<crate::probe_log::ProbePointName>,
    first: crate::probe_log::CsvPoll,
    range: ProbeRange,
) -> impl Stream<Item = Result<String, Infallible>> {
    let mut first_chunk = probe_log::csv_header();
    for rec in &first.records {
        if range.contains(rec.virtual_time_ns) {
            first_chunk.push_str(&probe_log::csv_record_line(&catalog, rec));
        }
    }
    stream::unfold(
        (
            backend,
            id,
            request_id,
            catalog,
            range,
            Some(first_chunk),
            first.done,
        ),
        |(backend, id, request_id, catalog, range, first_chunk, done)| async move {
            if let Some(chunk) = first_chunk {
                return Some((
                    Ok(chunk),
                    (backend, id, request_id, catalog, range, None, done),
                ));
            }
            if done {
                return None;
            }
            loop {
                let backend_poll = backend.clone();
                let id_poll = id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    backend_poll.csv_poll(&id_poll, request_id)
                })
                .await;
                match result {
                    Ok(Ok(poll)) if poll.records.is_empty() && !poll.done => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Ok(Ok(poll)) => {
                        let mut chunk = String::new();
                        for rec in &poll.records {
                            if range.contains(rec.virtual_time_ns) {
                                chunk.push_str(&probe_log::csv_record_line(&catalog, rec));
                            }
                        }
                        if chunk.is_empty() {
                            if poll.done {
                                return None;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            continue;
                        }
                        return Some((
                            Ok(chunk),
                            (backend, id, request_id, catalog, range, None, poll.done),
                        ));
                    }
                    _ => return None,
                }
            }
        },
    )
}

#[derive(serde::Deserialize)]
struct CsvStatusQuery {
    request_id: u64,
    viewer: Option<String>,
}

async fn probes_csv_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<CsvStatusQuery>,
) -> Result<Json<CsvStatusBody>, ApiError> {
    require_viewer(
        &state,
        &id,
        &headers,
        &ViewerParam {
            viewer: query.viewer,
        },
    )
    .await?;
    let backend = state.backend.clone();
    let poll = tokio::task::spawn_blocking(move || backend.csv_status(&id, query.request_id))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(Json(CsvStatusBody {
        failed: poll.failed,
        done: poll.done,
    }))
}

#[derive(serde::Serialize)]
struct CsvStatusBody {
    failed: bool,
    done: bool,
}

async fn ensure_exists(state: &AppState, id: &str) -> Result<(), ApiError> {
    let backend = state.backend.clone();
    let id = id.to_string();
    let exists = tokio::task::spawn_blocking(move || backend.exists(&id))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    if !exists {
        return Err(ApiError::not_found("unknown simulation"));
    }
    Ok(())
}

/// Serve with axum on `listen` (`host:port`).
pub async fn serve(
    listen: &str,
    backend: Arc<dyn SimulationBackend>,
) -> Result<(), std::io::Error> {
    let app = router(backend);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;
    eprintln!("cluster-server listening on http://{addr}/");
    axum::serve(listener, app).await
}
