//! Helpers for tests that drive a [`crate::CommandHost`] over REST
//! (`test-support` feature).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use contract::{ClusterId, sessions};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Send one request; returns the status and the JSON body (`Null` if empty,
/// a string if not JSON, e.g. axum's own rejection text).
///
/// # Panics
/// If the request cannot be built or the body cannot be read.
pub async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .expect("request");
    let response = router.clone().oneshot(request).await.expect("router");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

/// `POST /sessions` body: boards `boards` with no edges; this host runs `nodes`.
#[must_use]
pub fn sessions_body(boards: &[&str], nodes: &[&str]) -> Value {
    let boards: Vec<_> = boards
        .iter()
        .map(|id| json!({"id": id, "kind": "rl78", "endpoints": []}))
        .collect();
    let nodes: Vec<_> = nodes
        .iter()
        .map(|id| contract::node_key(id).to_string())
        .collect();
    json!({
        "topology": {"boards": boards, "edges": [], "margin_ns": 1000},
        "nodes": nodes,
    })
}

/// Create a session and return its id.
///
/// # Panics
/// If the session is not created.
pub async fn create_session(router: &Router, boards: &[&str], nodes: &[&str]) -> ClusterId {
    let (status, reply) = call(
        router,
        "POST",
        sessions::ROUTE,
        Some(sessions_body(boards, nodes)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reply}");
    serde_json::from_value(reply["cluster_id"].clone()).expect("cluster_id")
}
