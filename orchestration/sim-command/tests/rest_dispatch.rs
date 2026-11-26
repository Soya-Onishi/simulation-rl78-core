//! REST → Command thread dispatch without a real Node (#147 C / F).
//!
//! The Build thread launches a shell script that only sleeps, so RR requests
//! to it fail; Node RR round trips are covered by `sim-node/tests/command_host.rs`.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use contract::{
    ClusterId, HashedId, MAX_NODE_PROBE_IDS, build_ensure, build_restart, build_status,
    build_terminate, node_command, node_key, probe_interest, sessions, start, status,
};
use serde_json::{Value, json};
use sim_command::test_support::{call, create_session, sessions_body};
use sim_command::{CommandConfig, CommandHost};
use tempfile::TempDir;

struct Host {
    dir: TempDir,
    host: CommandHost,
    router: Router,
}

fn dummy_node(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("dummy-node.sh");
    std::fs::write(&path, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn config(dir: &TempDir) -> CommandConfig {
    let mut config = CommandConfig::new(dir.path().join("work"), dummy_node(dir.path()));
    config.node_timeout = Duration::from_millis(100);
    config.terminate_timeout = Duration::from_millis(200);
    config
}

fn host() -> Host {
    let dir = tempfile::tempdir().unwrap();
    let host = CommandHost::spawn(config(&dir)).unwrap();
    let router = host.router();
    Host { dir, host, router }
}

fn error_code(reply: &Value) -> &str {
    reply["code"].as_str().unwrap_or_default()
}

fn running_pid(reply: &Value) -> u32 {
    assert_eq!(reply["status"]["lifecycle"], "running", "{reply}");
    u32::try_from(reply["status"]["pid"].as_u64().unwrap()).unwrap()
}

/// Process group id from `/proc/{pid}/stat`.
fn process_group(pid: u32) -> u32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let (_, rest) = stat.rsplit_once(") ").unwrap();
    rest.split(' ').nth(2).unwrap().parse().unwrap()
}

const A: &str = "board-a";

#[tokio::test]
async fn session_body_is_validated() {
    let h = host();
    let (code, reply) = call(
        &h.router,
        "POST",
        sessions::ROUTE,
        Some(sessions_body(&[A], &["other"])),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&reply), "bad_request");
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .contains("not a topology board")
    );

    let (code, _) = call(
        &h.router,
        "POST",
        sessions::ROUTE,
        Some(json!({"nodes": []})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn new_session_is_idle_and_unknown_session_is_not_found() {
    let h = host();
    let cluster = create_session(&h.router, &[A, "board-b"], &[A]).await;
    assert!(h.host.is_running());
    let (code, reply) = call(&h.router, "GET", &status::url(cluster), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(reply, json!({"ok": "status", "state": "idle"}));

    let (code, reply) = call(&h.router, "GET", &status::url(ClusterId::new(1, 2)), None).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&reply), "not_found");
}

#[tokio::test]
async fn malformed_path_ids_are_bad_request() {
    let h = host();
    for uri in [
        "/v1/command/sessions/not-a-cluster/status",
        "/v1/command/sessions/1-2/nodes/DEADBEEFCAFEBABE/build/status",
    ] {
        let (code, reply) = call(&h.router, "GET", uri, None).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(error_code(&reply), "bad_request");
    }
    let (code, _) = call(
        &h.router,
        "GET",
        "/v1/command/sessions/1-2/nodes/deadbeefcafebabe/build",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unregistered_nodes_are_not_found() {
    let h = host();
    let cluster = create_session(&h.router, &[A, "board-b"], &[A]).await;
    let start_body = Some(json!({"command": {"type": "start"}}));
    let outside = node_command::url(cluster, node_key("board-b"));
    let (code, reply) = call(&h.router, "POST", &outside, start_body.clone()).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .contains("not in session")
    );

    let unbuilt = node_command::url(cluster, node_key(A));
    let (code, reply) = call(&h.router, "POST", &unbuilt, start_body).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert!(reply["message"].as_str().unwrap().contains("not built"));
}

#[tokio::test]
async fn too_many_probe_ids_are_bad_request() {
    let h = host();
    let cluster = create_session(&h.router, &[A], &[A]).await;
    let key = node_key(A);
    let ids: Vec<_> = (0..=MAX_NODE_PROBE_IDS as u64).map(HashedId::new).collect();

    let body = json!({"command": {"type": "probe_subscribe", "probe_ids": ids}});
    let (code, reply) = call(
        &h.router,
        "POST",
        &node_command::url(cluster, key),
        Some(body),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(reply["message"].as_str().unwrap().contains("TooManyProbes"));

    let body = json!({"probe_ids": ids});
    let (code, _) = call(
        &h.router,
        "POST",
        &probe_interest::url(cluster, key),
        Some(body),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn start_requires_every_node_built_and_rejects_until_ns() {
    let h = host();
    let cluster = create_session(&h.router, &[A], &[A]).await;
    let (code, reply) = call(&h.router, "POST", &start::url(cluster), Some(json!({}))).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(reply["message"].as_str().unwrap().contains("not built"));

    let (code, _) = call(
        &h.router,
        "POST",
        &start::url(cluster),
        Some(json!({"until_ns": 5})),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn build_ensure_restart_terminate_and_status() {
    let h = host();
    let cluster = create_session(&h.router, &[A], &[A]).await;
    let key = node_key(A);
    let status_url = build_status::url(cluster, key);

    let (_, reply) = call(&h.router, "GET", &status_url, None).await;
    assert_eq!(reply["status"]["lifecycle"], "not_started");

    let (code, reply) = call(&h.router, "POST", &build_ensure::url(cluster, key), None).await;
    assert_eq!(code, StatusCode::OK, "{reply}");
    assert_eq!(reply, json!({"ok": "ack"}));
    let pid = running_pid(&call(&h.router, "GET", &status_url, None).await.1);
    assert_eq!(
        process_group(pid),
        pid,
        "node must lead its own process group"
    );

    call(&h.router, "POST", &build_ensure::url(cluster, key), None).await;
    assert_eq!(
        running_pid(&call(&h.router, "GET", &status_url, None).await.1),
        pid
    );

    let (code, _) = call(&h.router, "POST", &build_restart::url(cluster, key), None).await;
    assert_eq!(code, StatusCode::OK);
    let restarted = running_pid(&call(&h.router, "GET", &status_url, None).await.1);
    assert_ne!(restarted, pid);

    let (code, _) = call(&h.router, "POST", &build_terminate::url(cluster, key), None).await;
    assert_eq!(code, StatusCode::OK);
    let (_, reply) = call(&h.router, "GET", &status_url, None).await;
    // The dummy ignores `Shutdown`, so terminate kills it (no exit code).
    assert_eq!(
        reply["status"],
        json!({"lifecycle": "exited", "code": null})
    );
}

#[tokio::test]
async fn shutdown_terminates_nodes_and_removes_session_dirs() {
    let h = host();
    let cluster = create_session(&h.router, &[A], &[A]).await;
    let key = node_key(A);
    call(&h.router, "POST", &build_ensure::url(cluster, key), None).await;
    let pid = running_pid(
        &call(&h.router, "GET", &build_status::url(cluster, key), None)
            .await
            .1,
    );
    let session_root = config(&h.dir).session_root(cluster);
    assert!(session_root.is_dir());

    h.host.shutdown().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "node {pid} survived shutdown"
    );
    assert!(!session_root.exists());
}

#[test]
fn spawn_removes_stale_session_dirs_only() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    let stale = config.work_dir.join("1-2");
    let unrelated = config.work_dir.join("not-a-session");
    for dir in [&stale, &unrelated] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("host_ctrl"), b"stale").unwrap();
    }

    let host = CommandHost::spawn(config).unwrap();
    assert!(!stale.exists());
    assert!(unrelated.join("host_ctrl").exists());
    host.shutdown().unwrap();
}
