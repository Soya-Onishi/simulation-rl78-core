//! HTTP client [`SimulationBackend`] for `cluster-backend`.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::contract::{
    ActionList, ApiError, CreateSimulationRequest, CreateSimulationResponse, ErrorBody,
    InvokeActionRequest, LogEvent, ProbeSamplesResponse, StartSimulationRequest, StatusEvent,
};
use crate::inspect::{InspectReply, InspectRequest};
use crate::orchestrate::{
    Phase1CreateRequest, Phase1CreateResponse, Phase2ConnectRequest, Phase2ConnectResponse,
};
use crate::probe_log::{
    CoarseBatch, CsvPoll, ProbeCatalogItem, ProbeRange, ProbeRef, ProbeWatchView,
};
use crate::topology::LogicalTopology;

use super::SimulationBackend;
use super::wire::{CsvStartBody, ExistsBody, HealthBody, HighWaterBody, ViewerTokenBody};

const RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// HTTP client for one `cluster-backend`.
#[derive(Clone)]
pub struct TcpipBackend {
    base: String,
}

impl TcpipBackend {
    #[must_use]
    pub fn new(base: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            base: trim_slash(base.into()),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        read_json(get(&self.url(path), RPC_TIMEOUT)?)
    }

    fn post_json<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<ureq::http::Response<ureq::Body>, ApiError> {
        post_json(&self.url(path), body, RPC_TIMEOUT)
    }

    fn post(&self, path: &str) -> Result<ureq::http::Response<ureq::Body>, ApiError> {
        post(&self.url(path), RPC_TIMEOUT)
    }

    fn put_json<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<ureq::http::Response<ureq::Body>, ApiError> {
        put_json(&self.url(path), body, RPC_TIMEOUT)
    }

    fn delete(&self, path: &str) -> Result<ureq::http::Response<ureq::Body>, ApiError> {
        delete(&self.url(path), RPC_TIMEOUT)
    }
}

/// `GET /internal/health` on `http://{ip}:{port}`.
pub fn fetch_health(ip: &str, port: u16, timeout: Duration) -> Result<HealthBody, String> {
    let url = format!("http://{ip}:{port}/internal/health");
    let response = get(&url, timeout).map_err(|err| err.to_string())?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("HTTP {status} from {url}"));
    }
    let body: HealthBody = read_json(response).map_err(|err| err.to_string())?;
    if body.status != "ok" {
        return Err(format!("unexpected health body from {url}"));
    }
    Ok(body)
}

/// `GET /internal/health` on `http://{ip}:{port}`. HTTP 200 and `status=ok`.
pub fn check_health(ip: &str, port: u16, timeout: Duration) -> Result<(), String> {
    fetch_health(ip, port, timeout).map(|_| ())
}

fn trim_slash(mut base: String) -> String {
    while base.ends_with('/') {
        base.pop();
    }
    base
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

fn get(url: &str, timeout: Duration) -> Result<ureq::http::Response<ureq::Body>, ApiError> {
    agent(timeout)
        .get(url)
        .call()
        .map_err(|err| ApiError::internal(err.to_string()))
}

fn delete(url: &str, timeout: Duration) -> Result<ureq::http::Response<ureq::Body>, ApiError> {
    agent(timeout)
        .delete(url)
        .call()
        .map_err(|err| ApiError::internal(err.to_string()))
}

fn post(url: &str, timeout: Duration) -> Result<ureq::http::Response<ureq::Body>, ApiError> {
    agent(timeout)
        .post(url)
        .send_empty()
        .map_err(|err| ApiError::internal(err.to_string()))
}

fn encode_json_body<B: Serialize>(body: &B) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(body).map_err(|err| ApiError::internal(format!("encode JSON: {err}")))
}

fn post_json<B: Serialize>(
    url: &str,
    body: &B,
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>, ApiError> {
    let bytes = encode_json_body(body)?;
    agent(timeout)
        .post(url)
        .header("Content-Type", "application/json")
        .send(bytes)
        .map_err(|err| ApiError::internal(err.to_string()))
}

fn put_json<B: Serialize>(
    url: &str,
    body: &B,
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>, ApiError> {
    let bytes = encode_json_body(body)?;
    agent(timeout)
        .put(url)
        .header("Content-Type", "application/json")
        .send(bytes)
        .map_err(|err| ApiError::internal(err.to_string()))
}

fn read_json<T: DeserializeOwned>(
    mut response: ureq::http::Response<ureq::Body>,
) -> Result<T, ApiError> {
    let status = response.status().as_u16();
    let bytes = response
        .body_mut()
        .read_to_vec()
        .map_err(|err| ApiError::internal(err.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(remote_error(status, &bytes));
    }
    serde_json::from_slice(&bytes)
        .map_err(|err| ApiError::internal(format!("invalid backend JSON: {err}")))
}

fn drop_body(mut response: ureq::http::Response<ureq::Body>) -> Result<(), ApiError> {
    let status = response.status().as_u16();
    let bytes = response
        .body_mut()
        .read_to_vec()
        .map_err(|err| ApiError::internal(err.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(remote_error(status, &bytes));
    }
    Ok(())
}

fn remote_error(status: u16, bytes: &[u8]) -> ApiError {
    match serde_json::from_slice::<ErrorBody>(bytes) {
        Ok(body) => ApiError::from_remote(status, &body),
        Err(_) => ApiError::internal(format!(
            "backend HTTP {status}: {}",
            String::from_utf8_lossy(bytes)
        )),
    }
}

impl SimulationBackend for TcpipBackend {
    fn create(&self, topology: LogicalTopology) -> Result<String, ApiError> {
        let body: CreateSimulationResponse = read_json(self.post_json(
            "/internal/simulations",
            &CreateSimulationRequest { topology },
        )?)?;
        Ok(body.id)
    }

    fn start(&self, id: &str, until_ns: Option<u64>) -> Result<(), ApiError> {
        drop_body(self.post_json(
            &format!("/internal/simulations/{id}/start"),
            &StartSimulationRequest { until_ns },
        )?)
    }

    fn stop(&self, id: &str) -> Result<(), ApiError> {
        drop_body(self.post(&format!("/internal/simulations/{id}/stop"))?)
    }

    fn reset(&self, id: &str) -> Result<(), ApiError> {
        drop_body(self.post(&format!("/internal/simulations/{id}/reset"))?)
    }

    fn shutdown(&self, id: &str) -> Result<(), ApiError> {
        drop_body(self.delete(&format!("/internal/simulations/{id}"))?)
    }

    fn exists(&self, id: &str) -> Result<bool, ApiError> {
        Ok(self
            .get_json::<ExistsBody>(&format!("/internal/simulations/{id}/exists"))?
            .exists)
    }

    fn status(&self, id: &str) -> Result<StatusEvent, ApiError> {
        self.get_json(&format!("/internal/simulations/{id}/status"))
    }

    fn topology(&self, id: &str) -> Result<crate::contract::TopologyView, ApiError> {
        self.get_json(&format!("/internal/simulations/{id}/topology"))
    }

    fn logs_after(&self, id: &str, after: u64) -> Result<Vec<LogEvent>, ApiError> {
        self.get_json(&format!("/internal/simulations/{id}/logs?after={after}"))
    }

    fn log_high_water(&self, id: &str) -> Result<u64, ApiError> {
        Ok(self
            .get_json::<HighWaterBody>(&format!("/internal/simulations/{id}/logs/high-water"))?
            .seq)
    }

    fn actions(&self, id: &str) -> Result<ActionList, ApiError> {
        self.get_json(&format!("/internal/simulations/{id}/actions"))
    }

    fn invoke_action(&self, id: &str, request: InvokeActionRequest) -> Result<(), ApiError> {
        drop_body(self.post_json(&format!("/internal/simulations/{id}/actions"), &request)?)
    }

    fn probe_catalog(&self, id: &str) -> Result<Vec<ProbeCatalogItem>, ApiError> {
        self.get_json(&format!("/internal/simulations/{id}/probes/catalog"))
    }

    fn probe_watch_get(&self, id: &str) -> Result<ProbeWatchView, ApiError> {
        self.get_json(&format!("/internal/simulations/{id}/probes/watch"))
    }

    fn probe_watch_put(&self, id: &str, probes: Vec<ProbeRef>) -> Result<ProbeWatchView, ApiError> {
        read_json(self.put_json(&format!("/internal/simulations/{id}/probes/watch"), &probes)?)
    }

    fn probe_watch_clear(&self, id: &str) -> Result<(), ApiError> {
        drop_body(self.delete(&format!("/internal/simulations/{id}/probes/watch"))?)
    }

    fn probe_watch_live(&self, id: &str, after: u64) -> Result<Vec<CoarseBatch>, ApiError> {
        self.get_json(&format!(
            "/internal/simulations/{id}/probes/live?after={after}"
        ))
    }

    fn csv_start(
        &self,
        id: &str,
        range: ProbeRange,
    ) -> Result<(u64, Vec<crate::probe_log::ProbePointName>), ApiError> {
        let body: CsvStartBody = read_json(self.post_json(
            &format!("/internal/simulations/{id}/probes/csv/start"),
            &range,
        )?)?;
        Ok((body.request_id, body.probes))
    }

    fn csv_poll(&self, id: &str, request_id: u64) -> Result<CsvPoll, ApiError> {
        self.get_json(&format!(
            "/internal/simulations/{id}/probes/csv/{request_id}/poll"
        ))
    }

    fn csv_status(&self, id: &str, request_id: u64) -> Result<CsvPoll, ApiError> {
        self.get_json(&format!(
            "/internal/simulations/{id}/probes/csv/{request_id}/status"
        ))
    }

    fn probe_samples(
        &self,
        id: &str,
        board: &str,
        probe: &str,
        from_ns: Option<u64>,
        to_ns: Option<u64>,
    ) -> Result<ProbeSamplesResponse, ApiError> {
        let mut path =
            format!("/internal/simulations/{id}/probes/samples?board={board}&probe={probe}");
        if let Some(from_ns) = from_ns {
            path.push_str(&format!("&from_ns={from_ns}"));
        }
        if let Some(to_ns) = to_ns {
            path.push_str(&format!("&to_ns={to_ns}"));
        }
        self.get_json(&path)
    }

    fn viewer_attach(&self, id: &str) -> Result<String, ApiError> {
        let body: ViewerTokenBody =
            read_json(self.post(&format!("/internal/simulations/{id}/viewer/attach"))?)?;
        Ok(body.token)
    }

    fn viewer_check(&self, id: &str, token: &str) -> Result<(), ApiError> {
        drop_body(self.post_json(
            &format!("/internal/simulations/{id}/viewer/check"),
            &ViewerTokenBody {
                token: token.to_string(),
            },
        )?)
    }

    fn viewer_detach(&self, id: &str, token: &str) -> Result<(), ApiError> {
        drop_body(self.post_json(
            &format!("/internal/simulations/{id}/viewer/detach"),
            &ViewerTokenBody {
                token: token.to_string(),
            },
        )?)
    }

    fn inspect(
        &self,
        id: &str,
        board_id: &str,
        req: InspectRequest,
    ) -> Result<InspectReply, ApiError> {
        read_json(self.post_json(
            &format!("/internal/simulations/{id}/boards/{board_id}/inspect"),
            &req,
        )?)
    }

    fn step(&self, id: &str, board_id: &str) -> Result<(), ApiError> {
        drop_body(self.post(&format!(
            "/internal/simulations/{id}/boards/{board_id}/step"
        ))?)
    }

    fn create_phase1(&self, req: Phase1CreateRequest) -> Result<Phase1CreateResponse, ApiError> {
        read_json(self.post_json("/internal/cluster/simulations", &req)?)
    }

    fn connect_phase2(&self, req: Phase2ConnectRequest) -> Result<Phase2ConnectResponse, ApiError> {
        read_json(self.post_json(
            &format!("/internal/cluster/simulations/{}/connect", req.id),
            &req,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::backend::BackendInfo;
    use crate::backend::serve::router;
    use crate::contract::ActionList;
    use crate::host::SessionNode;
    use crate::probe_log::{ProbeKind, ProbePointName, ProbeRecord};

    use super::*;

    struct MockBackend {
        last: Mutex<String>,
        /// Mock identity used in two-phase create ids and event names.
        name: String,
        /// Ordered call log shared between several mocks.
        events: Arc<Mutex<Vec<String>>>,
        /// Phase-1 request bodies as sent over the wire.
        phase1_bodies: Mutex<Vec<String>>,
        /// Remaining `shutdown` calls that fail with a 500.
        shutdown_failures: std::sync::atomic::AtomicU32,
        /// Ids this mock accepted in phase 1, besides the fixed `sim-1`.
        /// An id is removed again when `shutdown` succeeds for it.
        created_ids: Mutex<Vec<String>>,
        /// Remaining `create_phase1` calls that fail with a 500 (nothing is created).
        phase1_failures: std::sync::atomic::AtomicU32,
        /// Remaining `connect_phase2` calls that fail with a 500.
        phase2_failures: std::sync::atomic::AtomicU32,
        /// Run state reported by `status`: `running` after `start`, `stopped`
        /// after `stop` / `reset` and initially.
        run_state: Mutex<String>,
    }

    /// Consumes one pending injected failure of `counter`; `true` when the
    /// call must fail.
    fn take_failure(counter: &std::sync::atomic::AtomicU32) -> bool {
        counter
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_ok()
    }

    impl MockBackend {
        fn new() -> Arc<Self> {
            Self::named("mock", &Arc::new(Mutex::new(Vec::new())))
        }

        fn named(name: &str, events: &Arc<Mutex<Vec<String>>>) -> Arc<Self> {
            Arc::new(Self {
                last: Mutex::new(String::new()),
                name: name.into(),
                events: events.clone(),
                phase1_bodies: Mutex::new(Vec::new()),
                shutdown_failures: std::sync::atomic::AtomicU32::new(0),
                created_ids: Mutex::new(Vec::new()),
                phase1_failures: std::sync::atomic::AtomicU32::new(0),
                phase2_failures: std::sync::atomic::AtomicU32::new(0),
                run_state: Mutex::new("stopped".into()),
            })
        }

        fn set(&self, op: &str) {
            *self.last.lock().unwrap() = op.to_string();
        }

        /// Appends `"{op}:{mock name}:{simulation id}"` to the shared ordered
        /// call log.
        fn note(&self, op: &str, id: &str) {
            self.events
                .lock()
                .unwrap()
                .push(format!("{op}:{}:{id}", self.name));
        }

        /// `true` for the fixed id `sim-1` and for ids accepted in phase 1.
        fn knows(&self, id: &str) -> bool {
            id == "sim-1" || self.created_ids.lock().unwrap().iter().any(|k| k == id)
        }
    }

    impl SimulationBackend for MockBackend {
        fn create(&self, _topology: LogicalTopology) -> Result<String, ApiError> {
            self.set("create");
            Ok("sim-1".into())
        }
        fn start(&self, id: &str, until_ns: Option<u64>) -> Result<(), ApiError> {
            self.set(&format!("start:{id}:{}", until_ns.unwrap_or(0)));
            self.note(&format!("start({})", until_ns.unwrap_or(0)), id);
            *self.run_state.lock().unwrap() = "running".into();
            Ok(())
        }
        fn stop(&self, id: &str) -> Result<(), ApiError> {
            self.set(&format!("stop:{id}"));
            self.note("stop", id);
            *self.run_state.lock().unwrap() = "stopped".into();
            Ok(())
        }
        fn reset(&self, id: &str) -> Result<(), ApiError> {
            self.set(&format!("reset:{id}"));
            self.note("reset", id);
            *self.run_state.lock().unwrap() = "stopped".into();
            Ok(())
        }
        fn shutdown(&self, id: &str) -> Result<(), ApiError> {
            self.set(&format!("delete:{id}"));
            self.events.lock().unwrap().push(format!("delete:{id}"));
            let failing = self
                .shutdown_failures
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_ok();
            if failing {
                return Err(ApiError::internal("shutdown refused"));
            }
            self.created_ids.lock().unwrap().retain(|k| k != id);
            Ok(())
        }
        fn create_phase1(
            &self,
            req: Phase1CreateRequest,
        ) -> Result<Phase1CreateResponse, ApiError> {
            self.events
                .lock()
                .unwrap()
                .push(format!("phase1:{}", self.name));
            self.phase1_bodies
                .lock()
                .unwrap()
                .push(serde_json::to_string(&req).unwrap());
            if take_failure(&self.phase1_failures) {
                return Err(ApiError::internal("phase 1 refused"));
            }
            self.created_ids.lock().unwrap().push(req.id.clone());
            let multi = req.assignment.participating_backends().len() > 1;
            Ok(Phase1CreateResponse {
                id: req.id.clone(),
                endpoints: multi.then(|| crate::orchestrate::ArbiterEndpoints {
                    backend: req.local_backend.clone(),
                    advertise_ip: "127.0.0.1".into(),
                    control_tcp_port: 9000,
                    data_udp_port: 9001,
                }),
            })
        }
        fn connect_phase2(
            &self,
            req: Phase2ConnectRequest,
        ) -> Result<Phase2ConnectResponse, ApiError> {
            self.events.lock().unwrap().push(format!(
                "phase2:{}:peers={}:{}",
                self.name,
                req.peers.len(),
                req.multicast.ip
            ));
            if take_failure(&self.phase2_failures) {
                return Err(ApiError::internal("phase 2 refused"));
            }
            Ok(Phase2ConnectResponse { ok: true })
        }
        fn exists(&self, id: &str) -> Result<bool, ApiError> {
            Ok(self.knows(id))
        }
        fn status(&self, id: &str) -> Result<StatusEvent, ApiError> {
            if !self.knows(id) {
                return Err(ApiError::not_found("unknown simulation"));
            }
            self.note("status", id);
            let state = self.run_state.lock().unwrap().clone();
            Ok(StatusEvent {
                id: id.into(),
                state: state.clone(),
                virtual_time_ns: 0,
                stop_reason: None,
                nodes: vec![SessionNode {
                    id: "mcu-a".into(),
                    kind: "rl78".into(),
                    state,
                }],
            })
        }
        fn topology(&self, id: &str) -> Result<crate::contract::TopologyView, ApiError> {
            if !self.knows(id) {
                return Err(ApiError::not_found("unknown simulation"));
            }
            Ok(crate::contract::TopologyView::from_logical(&one_board()))
        }
        fn logs_after(&self, id: &str, _after: u64) -> Result<Vec<LogEvent>, ApiError> {
            self.note("logs", id);
            Ok(vec![])
        }
        fn log_high_water(&self, _id: &str) -> Result<u64, ApiError> {
            Ok(7)
        }
        fn actions(&self, id: &str) -> Result<ActionList, ApiError> {
            self.note("actions", id);
            Ok(ActionList { actions: vec![] })
        }
        fn invoke_action(&self, id: &str, request: InvokeActionRequest) -> Result<(), ApiError> {
            self.note(&format!("invoke({})", request.board_id), id);
            Ok(())
        }
        fn probe_catalog(&self, id: &str) -> Result<Vec<ProbeCatalogItem>, ApiError> {
            self.note("catalog", id);
            Ok(vec![])
        }
        fn probe_watch_get(&self, id: &str) -> Result<ProbeWatchView, ApiError> {
            self.note("watch_get", id);
            Ok(ProbeWatchView { probes: vec![] })
        }
        fn probe_watch_put(
            &self,
            id: &str,
            probes: Vec<ProbeRef>,
        ) -> Result<ProbeWatchView, ApiError> {
            self.note(&format!("watch_put({})", probes.len()), id);
            Ok(ProbeWatchView { probes })
        }
        fn probe_watch_clear(&self, id: &str) -> Result<(), ApiError> {
            self.note("watch_clear", id);
            Ok(())
        }
        fn probe_watch_live(&self, id: &str, after: u64) -> Result<Vec<CoarseBatch>, ApiError> {
            self.note(&format!("watch_live({after})"), id);
            Ok(vec![])
        }
        fn csv_start(
            &self,
            _id: &str,
            _range: ProbeRange,
        ) -> Result<(u64, Vec<ProbePointName>), ApiError> {
            Ok((
                3,
                vec![ProbePointName {
                    probe_id: 1,
                    board_id: "mcu-a".into(),
                    name: "p0".into(),
                    kind: ProbeKind::Digital,
                }],
            ))
        }
        fn csv_poll(&self, _id: &str, _request_id: u64) -> Result<CsvPoll, ApiError> {
            Ok(CsvPoll {
                records: vec![ProbeRecord {
                    probe_id: 1,
                    virtual_time_ns: 0,
                    kind: ProbeKind::Digital,
                    value: 1,
                }],
                done: true,
                failed: false,
                truncated: false,
            })
        }
        fn csv_status(&self, _id: &str, request_id: u64) -> Result<CsvPoll, ApiError> {
            self.csv_poll("sim-1", request_id)
        }
        fn probe_samples(
            &self,
            _id: &str,
            _board: &str,
            _probe: &str,
            _from_ns: Option<u64>,
            _to_ns: Option<u64>,
        ) -> Result<ProbeSamplesResponse, ApiError> {
            Ok(ProbeSamplesResponse {
                truncated: false,
                samples: vec![],
            })
        }
        fn viewer_attach(&self, _id: &str) -> Result<String, ApiError> {
            Ok("tok".into())
        }
        fn viewer_check(&self, _id: &str, token: &str) -> Result<(), ApiError> {
            if token == "tok" {
                Ok(())
            } else {
                Err(ApiError::viewer_busy())
            }
        }
        fn viewer_detach(&self, _id: &str, _token: &str) -> Result<(), ApiError> {
            Ok(())
        }
        fn inspect(
            &self,
            _id: &str,
            _board_id: &str,
            _req: InspectRequest,
        ) -> Result<InspectReply, ApiError> {
            Ok(InspectReply::Ok)
        }
        fn step(&self, _id: &str, _board_id: &str) -> Result<(), ApiError> {
            Ok(())
        }
    }

    fn one_board() -> LogicalTopology {
        LogicalTopology {
            boards: vec![crate::topology::BoardSpec {
                id: "mcu-a".into(),
                kind: "rl78".into(),
                elf: Some("base64:AA==".into()),
                endpoints: vec![],
            }],
            edges: vec![],
            margin_ns: 1000,
            headroom_threshold_ns: 200,
            ready_timeout_ms: 10_000,
        }
    }

    #[test]
    fn health_reports_running_boards_from_counter() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base, _join) = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let running = Arc::new(std::sync::atomic::AtomicU32::new(3));
            let app = router(MockBackend::new(), BackendInfo::for_tests(16), running);
            let join = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            (format!("http://{addr}"), join)
        });
        let port = url::Url::parse(&base).unwrap().port().unwrap();
        let health = fetch_health("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        assert_eq!(health.max_boards, 16);
        assert_eq!(health.running_boards, 3);
    }

    #[test]
    fn tcpip_round_trip_and_error() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base, _join) = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let running = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let app = router(MockBackend::new(), BackendInfo::for_tests(8), running);
            let join = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            (format!("http://{addr}"), join)
        });

        let backend = TcpipBackend::new(&base);
        let port = url::Url::parse(&base).unwrap().port().unwrap();
        let health = fetch_health("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        assert_eq!(health.status, "ok");
        assert_eq!(health.max_boards, 8);
        assert_eq!(health.running_boards, 0);
        check_health("127.0.0.1", port, Duration::from_secs(2)).unwrap();

        let id = backend.create(one_board()).unwrap();
        assert_eq!(id, "sim-1");
        backend.start(&id, Some(10)).unwrap();
        backend.stop(&id).unwrap();
        backend.reset(&id).unwrap();
        assert!(backend.exists(&id).unwrap());
        assert_eq!(backend.status(&id).unwrap().id, "sim-1");
        let topo = backend.topology(&id).unwrap();
        assert_eq!(topo.boards.len(), 1);
        assert_eq!(topo.boards[0].id, "mcu-a");
        assert!(topo.boards[0].kind == "rl78");
        assert!(backend.logs_after(&id, 0).unwrap().is_empty());
        assert_eq!(backend.log_high_water(&id).unwrap(), 7);
        assert!(backend.actions(&id).unwrap().actions.is_empty());
        backend
            .invoke_action(
                &id,
                InvokeActionRequest {
                    board_id: "mcu-a".into(),
                    device: "sw".into(),
                    action: "press".into(),
                    params: Default::default(),
                },
            )
            .unwrap();
        assert!(backend.probe_catalog(&id).unwrap().is_empty());
        let watch = backend
            .probe_watch_put(
                &id,
                vec![ProbeRef {
                    board: "mcu-a".into(),
                    name: "p0".into(),
                }],
            )
            .unwrap();
        assert_eq!(watch.probes.len(), 1);
        backend.probe_watch_get(&id).unwrap();
        backend.probe_watch_clear(&id).unwrap();
        backend.probe_watch_live(&id, 0).unwrap();
        let (req_id, probes) = backend
            .csv_start(
                &id,
                ProbeRange {
                    from_ns: None,
                    to_ns: None,
                },
            )
            .unwrap();
        assert_eq!(req_id, 3);
        assert_eq!(probes.len(), 1);
        assert!(backend.csv_poll(&id, req_id).unwrap().done);
        backend.csv_status(&id, req_id).unwrap();
        backend
            .probe_samples(&id, "mcu-a", "p0", None, None)
            .unwrap();
        let token = backend.viewer_attach(&id).unwrap();
        backend.viewer_check(&id, &token).unwrap();
        let busy = backend.viewer_check(&id, "nope").unwrap_err();
        assert_eq!(busy.code, "viewer_busy");
        backend.viewer_detach(&id, &token).unwrap();
        backend
            .inspect(&id, "mcu-a", InspectRequest::ReadRegs)
            .unwrap();
        backend.step(&id, "mcu-a").unwrap();
        backend.shutdown(&id).unwrap();

        let missing = backend.status("missing").unwrap_err();
        assert_eq!(missing.status, 404);
        assert_eq!(missing.code, "not_found");
    }

    fn serve_mock(
        rt: &tokio::runtime::Runtime,
        mock: Arc<MockBackend>,
        info: BackendInfo,
    ) -> crate::config::ServerAddr {
        let port = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let running = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let app = router(mock, info, running);
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            port
        });
        crate::config::ServerAddr {
            ip: "127.0.0.1".into(),
            port,
        }
    }

    fn two_inline_boards() -> LogicalTopology {
        let board = |id: &str, payload: &str| crate::topology::BoardSpec {
            id: id.into(),
            kind: "rl78".into(),
            elf: Some(format!("base64:{payload}")),
            endpoints: vec![],
        };
        LogicalTopology {
            boards: vec![board("a", "QUFB"), board("b", "QkJC")],
            ..one_board()
        }
    }

    #[test]
    fn phase_requests_round_trip_over_http() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let addr = serve_mock(
            &rt,
            MockBackend::named("x", &events),
            BackendInfo::for_tests(4),
        );
        let backend = TcpipBackend::new(format!("http://{}:{}", addr.ip, addr.port));

        let key = crate::orchestrate::backend_key(&addr.ip, addr.port);
        let reply = backend
            .create_phase1(Phase1CreateRequest {
                id: "sim-x".into(),
                topology: one_board(),
                assignment: crate::orchestrate::AssignmentTable {
                    entries: vec![crate::orchestrate::BoardAssignment {
                        board_id: "mcu-a".into(),
                        backend: key.clone(),
                    }],
                },
                local_backend: key,
            })
            .unwrap();
        assert_eq!(reply.id, "sim-x");
        assert_eq!(reply.endpoints, None);

        let ack = backend
            .connect_phase2(Phase2ConnectRequest {
                cluster_key: "sim-x".into(),
                id: reply.id,
                peers: vec![],
                multicast: crate::orchestrate::MulticastGroup {
                    ip: "239.255.0.0".into(),
                    port: 47000,
                },
            })
            .unwrap();
        assert!(ack.ok);

        // A body id that differs from the path id is rejected by the router.
        let url = format!(
            "http://{}:{}/internal/cluster/simulations/other/connect",
            addr.ip, addr.port
        );
        let body = Phase2ConnectRequest {
            id: "sim-x".into(),
            cluster_key: "sim-x".into(),
            peers: vec![],
            multicast: crate::orchestrate::MulticastGroup {
                ip: "239.255.0.0".into(),
                port: 47000,
            },
        };
        let err = read_json::<Phase2ConnectResponse>(
            post_json(&url, &body, Duration::from_secs(2)).unwrap(),
        )
        .unwrap_err();
        assert_eq!(err.status, 400);
    }

    /// Id of participant 1 given the client-facing id of participant 0.
    fn second_member_id(first: &str) -> String {
        let stem = first
            .strip_suffix("-0")
            .expect("first member id ends in -0");
        format!("{stem}-1")
    }

    #[test]
    fn multi_backend_create_runs_phases_in_order_and_fans_out_shutdown() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let m0 = MockBackend::named("s0", &events);
        let m1 = MockBackend::named("s1", &events);
        let servers = vec![
            serve_mock(&rt, m0.clone(), BackendInfo::for_tests(1)),
            serve_mock(&rt, m1.clone(), BackendInfo::for_tests(1)),
        ];
        let multi = crate::orchestrate::MultiBackend::new(servers);

        let id = multi.create(two_inline_boards()).unwrap();

        // The frontend chose the ids: the first participant's id is the client's,
        // the second participant's differs only in the trailing index.
        let id1 = second_member_id(&id);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "phase1:s0",
                "phase1:s1",
                "phase2:s0:peers=1:239.255.0.0",
                "phase2:s1:peers=1:239.255.0.0",
            ]
        );
        // Only the owner's inline firmware crosses the wire.
        let s0 = m0.phase1_bodies.lock().unwrap()[0].clone();
        let s1 = m1.phase1_bodies.lock().unwrap()[0].clone();
        assert!(s0.contains("QUFB") && !s0.contains("QkJC"));
        assert!(s1.contains("QkJC") && !s1.contains("QUFB"));

        events.lock().unwrap().clear();
        multi.shutdown(&id).unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec![format!("delete:{id}"), format!("delete:{id1}")]
        );
    }

    #[test]
    fn multi_backend_shutdown_reaches_every_backend_and_retries_only_failures() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let m0 = MockBackend::named("s0", &events);
        let m1 = MockBackend::named("s1", &events);
        let servers = vec![
            serve_mock(&rt, m0.clone(), BackendInfo::for_tests(1)),
            serve_mock(&rt, m1.clone(), BackendInfo::for_tests(1)),
        ];
        let multi = crate::orchestrate::MultiBackend::new(servers);
        let id = multi.create(two_inline_boards()).unwrap();

        // The first backend (the primary) refuses twice: the first attempt and
        // the one automatic retry. The second backend must still be destroyed.
        m0.shutdown_failures
            .store(2, std::sync::atomic::Ordering::SeqCst);
        events.lock().unwrap().clear();
        let err = multi.shutdown(&id).unwrap_err();
        assert_eq!(err.status, 500);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                format!("delete:{id}"),
                format!("delete:{id}"),
                format!("delete:{}", second_member_id(&id))
            ]
        );

        // While part of the simulation remains, its multicast group stays reserved.
        let blocked = multi.create(two_inline_boards()).unwrap();
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e == "phase2:s0:peers=1:239.255.0.1"),
            "{:?}",
            events.lock().unwrap()
        );
        multi.shutdown(&blocked).unwrap();

        // Repeating the DELETE touches only the backend that failed, then frees
        // the group for reuse.
        events.lock().unwrap().clear();
        multi.shutdown(&id).unwrap();
        assert_eq!(*events.lock().unwrap(), vec![format!("delete:{id}")]);
        events.lock().unwrap().clear();
        multi.create(two_inline_boards()).unwrap();
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e == "phase2:s0:peers=1:239.255.0.0")
        );
    }

    /// One server and one board: `MultiBackend` behaves like a plain single
    /// backend. Every operation reaches that backend exactly once under the id
    /// the client holds, and phase 2 (peer connect) is never sent.
    #[test]
    fn multi_backend_with_one_server_runs_the_whole_lifecycle_without_phase2() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let m0 = MockBackend::named("s0", &events);
        let servers = vec![serve_mock(&rt, m0.clone(), BackendInfo::for_tests(4))];
        let multi = crate::orchestrate::MultiBackend::new(servers);

        let id = multi.create(one_board()).unwrap();
        assert_eq!(*events.lock().unwrap(), vec!["phase1:s0"]);

        assert_eq!(multi.status(&id).unwrap().id, id);
        multi.start(&id, Some(500)).unwrap();
        multi.stop(&id).unwrap();
        multi.reset(&id).unwrap();
        assert!(multi.probe_catalog(&id).unwrap().is_empty());
        assert!(multi.actions(&id).unwrap().actions.is_empty());
        multi
            .invoke_action(
                &id,
                InvokeActionRequest {
                    board_id: "mcu-a".into(),
                    device: "sw".into(),
                    action: "press".into(),
                    params: Default::default(),
                },
            )
            .unwrap();
        let probe = ProbeRef {
            board: "mcu-a".into(),
            name: "p0".into(),
        };
        assert_eq!(
            multi
                .probe_watch_put(&id, vec![probe])
                .unwrap()
                .probes
                .len(),
            1
        );
        multi.probe_watch_get(&id).unwrap();
        assert!(multi.probe_watch_live(&id, 0).unwrap().is_empty());
        multi.probe_watch_clear(&id).unwrap();
        assert!(multi.logs_after(&id, 0).unwrap().is_empty());
        multi.shutdown(&id).unwrap();

        // The backend saw each call once, in order, under the client's id, and
        // phase 2 is absent.
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "phase1:s0".to_string(),
                format!("status:s0:{id}"),
                format!("start(500):s0:{id}"),
                format!("stop:s0:{id}"),
                format!("reset:s0:{id}"),
                format!("catalog:s0:{id}"),
                format!("actions:s0:{id}"),
                format!("invoke(mcu-a):s0:{id}"),
                format!("watch_put(1):s0:{id}"),
                format!("watch_get:s0:{id}"),
                format!("watch_live(0):s0:{id}"),
                format!("watch_clear:s0:{id}"),
                format!("logs:s0:{id}"),
                format!("delete:{id}"),
            ]
        );
        assert!(
            !events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("phase2")),
            "{:?}",
            events.lock().unwrap()
        );
    }

    #[test]
    fn multi_backend_rejects_setting_mismatch_without_creating_anything() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut odd = BackendInfo::for_tests(1);
        odd.time_sync_period_ms = 250;
        let servers = vec![
            serve_mock(
                &rt,
                MockBackend::named("s0", &events),
                BackendInfo::for_tests(1),
            ),
            serve_mock(&rt, MockBackend::named("s1", &events), odd),
        ];
        let multi = crate::orchestrate::MultiBackend::new(servers);

        let err = multi.create(two_inline_boards()).unwrap_err();

        assert_eq!(err.code, "config_mismatch");
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn multi_backend_rejects_when_capacity_is_short() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let servers = vec![serve_mock(
            &rt,
            MockBackend::named("s0", &events),
            BackendInfo::for_tests(1),
        )];
        let multi = crate::orchestrate::MultiBackend::new(servers);

        let err = multi.create(two_inline_boards()).unwrap_err();

        assert_eq!(err.code, "insufficient_capacity");
        assert!(events.lock().unwrap().is_empty());
    }

    /// Task 19: two `cluster-backend` HTTP servers on one host (loopback).
    ///
    /// Each server is the real `backend::serve::router` backed by a
    /// [`MockBackend`]; `MultiBackend` talks to them through real `TcpipBackend`
    /// HTTP clients, exactly as `simulation-frontend` does. No board binary,
    /// arbiter process or iceoryx2 service is started, so a "leftover" is a
    /// simulation id that a [`MockBackend`] accepted in phase 1 and that was
    /// not removed by a successful `shutdown` (see `live`).
    ///
    /// Covered here: board assignment, phase 1 then phase 2, start / stop
    /// fan-out, merged status, destroy fan-out, rollback after a phase 1 or
    /// phase 2 failure, and an incomplete rollback finished by a later DELETE.
    ///
    /// Covered elsewhere with real sockets (run them together with
    /// `cargo test -p sim-cluster --lib -- dual_backend_integration
    /// host_stop_reaches_every_peer_once_and_is_not_echoed
    /// cross_machine_uart_and_analog_reach_the_peer_node_and_same_host_stays_direct`):
    ///
    /// - Stop propagation between backends: the control TCP mesh test
    ///   `cluster_net::control_tcp::tests::host_stop_reaches_every_peer_once_and_is_not_echoed`
    ///   (one arbiter publishes `HostStop`; every peer arbiter receives it once and
    ///   it is not echoed back).
    /// - Edges that cross backends: `edge_proxy::tests::
    ///   cross_machine_uart_and_analog_reach_the_peer_node_and_same_host_stays_direct`.
    ///
    /// Not covered automatically (manual checklist in `tasks/todo.md`, Task 19):
    /// real `cluster-backend` / `cluster-arbiter` / board processes, and the
    /// "no process or iceoryx2 file remains" check after a failed create.
    mod dual_backend_integration {
        use super::*;

        struct Dual {
            // Keeps the HTTP servers alive.
            _rt: tokio::runtime::Runtime,
            events: Arc<Mutex<Vec<String>>>,
            m0: Arc<MockBackend>,
            m1: Arc<MockBackend>,
            multi: Arc<crate::orchestrate::MultiBackend>,
        }

        /// Two servers with room for one board each.
        fn dual() -> Dual {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let events = Arc::new(Mutex::new(Vec::new()));
            let m0 = MockBackend::named("s0", &events);
            let m1 = MockBackend::named("s1", &events);
            let servers = vec![
                serve_mock(&rt, m0.clone(), BackendInfo::for_tests(1)),
                serve_mock(&rt, m1.clone(), BackendInfo::for_tests(1)),
            ];
            let multi = crate::orchestrate::MultiBackend::new(servers);
            Dual {
                _rt: rt,
                events,
                m0,
                m1,
                multi,
            }
        }

        impl Dual {
            fn take_events(&self) -> Vec<String> {
                std::mem::take(&mut *self.events.lock().unwrap())
            }

            /// Simulation ids still alive on each backend (accepted in phase 1
            /// and not destroyed).
            fn live(&self) -> (Vec<String>, Vec<String>) {
                (
                    self.m0.created_ids.lock().unwrap().clone(),
                    self.m1.created_ids.lock().unwrap().clone(),
                )
            }

            fn assert_nothing_left(&self) {
                assert_eq!(self.live(), (vec![], vec![]));
            }
        }

        fn sorted(mut events: Vec<String>) -> Vec<String> {
            events.sort();
            events
        }

        #[test]
        fn create_start_status_stop_and_destroy_leave_nothing_behind() {
            let d = dual();

            // Assignment + phase 1 (both backends) + phase 2 (both backends).
            let id = d.multi.create(two_inline_boards()).unwrap();
            let id1 = second_member_id(&id);
            assert_eq!(
                d.take_events(),
                vec![
                    "phase1:s0",
                    "phase1:s1",
                    "phase2:s0:peers=1:239.255.0.0",
                    "phase2:s1:peers=1:239.255.0.0",
                ]
            );
            assert_eq!(d.live(), (vec![id.clone()], vec![id1.clone()]));

            // Merged status before start: every member is stopped.
            let status = d.multi.status(&id).unwrap();
            assert_eq!(status.id, id);
            assert_eq!(status.state, "stopped");
            assert_eq!(status.nodes.len(), 2);
            d.take_events();

            // Start reaches both backends, each under its own id.
            d.multi.start(&id, Some(1000)).unwrap();
            assert_eq!(
                sorted(d.take_events()),
                vec![
                    format!("start(1000):s0:{id}"),
                    format!("start(1000):s1:{id1}")
                ]
            );
            assert_eq!(d.multi.status(&id).unwrap().state, "running");

            // One backend stopping on its own (what a node-originated HostStop
            // does after the arbiters exchanged it, see the module comment)
            // makes the merged simulation `stopped`.
            d.m1.stop(&id1).unwrap();
            assert_eq!(d.multi.status(&id).unwrap().state, "stopped");

            // Stop reaches both backends, each under its own id.
            d.multi.start(&id, None).unwrap();
            assert_eq!(d.multi.status(&id).unwrap().state, "running");
            d.take_events();
            d.multi.stop(&id).unwrap();
            assert_eq!(
                sorted(d.take_events()),
                vec![format!("stop:s0:{id}"), format!("stop:s1:{id1}")]
            );
            assert_eq!(d.multi.status(&id).unwrap().state, "stopped");

            // Destroy fans out and nothing remains on either backend.
            d.take_events();
            d.multi.shutdown(&id).unwrap();
            assert_eq!(
                d.take_events(),
                vec![format!("delete:{id}"), format!("delete:{id1}")]
            );
            d.assert_nothing_left();

            // The multicast group was released: the next create reuses it.
            let again = d.multi.create(two_inline_boards()).unwrap();
            assert!(
                d.take_events()
                    .iter()
                    .any(|e| e == "phase2:s0:peers=1:239.255.0.0")
            );
            d.multi.shutdown(&again).unwrap();
            d.assert_nothing_left();
        }

        #[test]
        fn phase2_failure_destroys_every_member_newest_first_and_frees_the_group() {
            let d = dual();
            d.m1.phase2_failures
                .store(1, std::sync::atomic::Ordering::SeqCst);

            let err = d.multi.create(two_inline_boards()).unwrap_err();

            assert_eq!(err.status, 500);
            assert!(err.message.contains("phase 2"), "{}", err.message);
            assert!(
                !err.message.contains("rollback incomplete"),
                "{}",
                err.message
            );
            let events = d.take_events();
            // The id of the first member is the only one the frontend ever
            // returns; it appears in the rollback in front of the second.
            let first_delete = events
                .iter()
                .position(|e| e.starts_with("delete:"))
                .expect("rollback deletes");
            assert_eq!(
                events[..first_delete],
                [
                    "phase1:s0",
                    "phase1:s1",
                    "phase2:s0:peers=1:239.255.0.0",
                    "phase2:s1:peers=1:239.255.0.0",
                ]
            );
            let deletes: Vec<_> = events[first_delete..].to_vec();
            assert_eq!(deletes.len(), 2, "{events:?}");
            assert!(deletes[0].ends_with("-1"), "newest first: {deletes:?}");
            assert!(deletes[1].ends_with("-0"), "newest first: {deletes:?}");
            d.assert_nothing_left();

            // The group is free again and the backends are reusable.
            let id = d.multi.create(two_inline_boards()).unwrap();
            assert!(
                d.take_events()
                    .iter()
                    .any(|e| e == "phase2:s0:peers=1:239.255.0.0")
            );
            d.multi.shutdown(&id).unwrap();
            d.assert_nothing_left();
        }

        #[test]
        fn phase1_failure_on_the_second_backend_rolls_back_the_first_and_skips_phase2() {
            let d = dual();
            d.m1.phase1_failures
                .store(1, std::sync::atomic::Ordering::SeqCst);

            let err = d.multi.create(two_inline_boards()).unwrap_err();

            assert_eq!(err.status, 500);
            assert!(err.message.contains("phase 1"), "{}", err.message);
            let events = d.take_events();
            assert_eq!(events.len(), 4, "{events:?}");
            assert_eq!(events[..2], ["phase1:s0", "phase1:s1"]);
            // The failed backend is asked to destroy the id too (its reply may
            // have been lost), after which nothing remains anywhere.
            assert!(events[2].ends_with("-1") && events[2].starts_with("delete:"));
            assert!(events[3].ends_with("-0") && events[3].starts_with("delete:"));
            assert!(
                !events.iter().any(|e| e.starts_with("phase2")),
                "{events:?}"
            );
            d.assert_nothing_left();
        }

        #[test]
        fn incomplete_rollback_is_reported_and_finished_by_a_later_delete() {
            let d = dual();
            d.m1.phase2_failures
                .store(1, std::sync::atomic::Ordering::SeqCst);
            // The first backend refuses the destroy and its one automatic retry.
            d.m0.shutdown_failures
                .store(2, std::sync::atomic::Ordering::SeqCst);

            let err = d.multi.create(two_inline_boards()).unwrap_err();

            assert!(
                err.message.contains("rollback incomplete"),
                "{}",
                err.message
            );
            let (live0, live1) = d.live();
            assert_eq!(live1, Vec::<String>::new(), "second backend was destroyed");
            assert_eq!(live0.len(), 1, "only the refusing backend remains");
            let leftover = live0[0].clone();
            assert!(err.message.contains(&leftover), "{}", err.message);

            // While it remains, its multicast group is not handed out again.
            let other = d.multi.create(two_inline_boards()).unwrap();
            assert!(
                d.take_events()
                    .iter()
                    .any(|e| e == "phase2:s0:peers=1:239.255.0.1")
            );
            d.multi.shutdown(&other).unwrap();

            // DELETE of the id named in the error destroys exactly that part.
            d.take_events();
            d.multi.shutdown(&leftover).unwrap();
            assert_eq!(d.take_events(), vec![format!("delete:{leftover}")]);
            d.assert_nothing_left();

            // Now the group is free again.
            let id = d.multi.create(two_inline_boards()).unwrap();
            assert!(
                d.take_events()
                    .iter()
                    .any(|e| e == "phase2:s0:peers=1:239.255.0.0")
            );
            d.multi.shutdown(&id).unwrap();
            d.assert_nothing_left();
        }
    }
}
