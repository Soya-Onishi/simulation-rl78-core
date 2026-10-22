//! Axum HTTP surface for the cluster control plane and embedded UI.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};

use crate::backend::{SimulationBackend, prepare_topology};
use crate::contract::{
    ActionList, ApiError, CreateSimulationRequest, CreateSimulationResponse, InvokeActionRequest,
};
use crate::probe_log::{self, ProbeRange};
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
        .route("/api/simulation/{id}/log", get(log_sse))
        .route("/api/simulation/{id}/probes", get(probe_sse))
        .route("/api/simulation/{id}/probes.csv", get(probes_csv))
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

async fn start_simulation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    run_command(state, id, |b, id| b.start(id)).await
}

async fn stop_simulation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    run_command(state, id, |b, id| b.stop(id)).await
}

async fn reset_simulation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    run_command(state, id, |b, id| b.reset(id)).await
}

async fn list_actions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ActionList>, ApiError> {
    let backend = state.backend.clone();
    let list = tokio::task::spawn_blocking(move || backend.actions(&id))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(Json(list))
}

async fn invoke_action(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<InvokeActionRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(request) =
        body.map_err(|_| ApiError::bad_request("bad_param", "invalid action parameter"))?;
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || backend.invoke_action(&id, request))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(StatusCode::OK)
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
) -> Result<StatusCode, ApiError> {
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || backend.delete(&id))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    Ok(StatusCode::OK)
}

async fn status_sse(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    ensure_exists(&state, &id).await?;
    let stream = status_event_stream(state.backend.clone(), id);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn log_sse(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    ensure_exists(&state, &id).await?;
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
}

async fn probe_sse(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    ensure_exists(&state, &id).await?;
    let stream = probe_event_stream(state.backend.clone(), id);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn probe_event_stream(
    backend: Arc<dyn SimulationBackend>,
    id: String,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold(
        (backend, id, 0u64, Vec::<Event>::new(), true),
        |(backend, id, mut after, mut pending, snapshot)| async move {
            loop {
                if let Some(event) = pending.pop() {
                    return Some((Ok(event), (backend, id, after, pending, false)));
                }
                let backend_poll = backend.clone();
                let id_poll = id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    backend_poll.probes_after(&id_poll, after, snapshot)
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
    Query(query): Query<ProbeCsvQuery>,
) -> Result<Response, ApiError> {
    if let Some(format) = query.format.as_deref()
        && format != "csv"
    {
        return Err(ApiError::bad_request("bad_param", "invalid probe format"));
    }
    let range = ProbeRange::parse(query.from_ns.as_deref(), query.to_ns.as_deref())
        .map_err(|message| ApiError::bad_request("bad_param", message))?;
    ensure_exists(&state, &id).await?;
    let backend = state.backend.clone();
    let id_export = id.clone();
    let export = tokio::task::spawn_blocking(move || backend.probe_export(&id_export))
        .await
        .map_err(|err| ApiError::internal(err.to_string()))??;
    let truncated = export.truncated;
    let csv = tokio::task::spawn_blocking(move || -> Result<String, ApiError> {
        let records =
            probe_log::load_export(&export).map_err(|err| ApiError::internal(err.to_string()))?;
        Ok(probe_log::render_csv(&export.catalog, &records, range))
    })
    .await
    .map_err(|err| ApiError::internal(err.to_string()))??;
    let body = Body::from(csv);
    let mut response = Response::new(body);
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"probes.csv\""),
    );
    if truncated {
        response
            .headers_mut()
            .insert("x-probe-truncated", HeaderValue::from_static("1"));
    }
    Ok(response)
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
