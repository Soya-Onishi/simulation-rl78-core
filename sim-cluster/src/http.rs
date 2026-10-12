//! Axum HTTP surface for the cluster control plane and embedded UI.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};

use crate::backend::{SimulationBackend, prepare_topology};
use crate::contract::{ApiError, CreateSimulationRequest, CreateSimulationResponse};
use crate::web::PAGE_HTML;

/// Same ceiling as the previous hand-rolled server. One `base64:` elf is at most
/// [`crate::elf_source::MAX_FIRMWARE_BYTES`] raw (~4/3 that once encoded).
const MAX_BODY: usize = 64 * 1024 * 1024;

const _: () = assert!(
    MAX_BODY
        >= crate::elf_source::MAX_FIRMWARE_BYTES * 4 / 3 + crate::elf_source::MAX_FIRMWARE_BYTES / 8
);

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
        .route("/api/simulation/{id}", delete(delete_simulation))
        .route("/api/simulation/{id}/status", get(status_sse))
        .route("/api/simulation/{id}/log", get(log_sse))
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
