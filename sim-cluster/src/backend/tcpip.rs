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
        read_json(send("GET", &self.url(path), None::<&()>, RPC_TIMEOUT)?)
    }

    fn send_json<B: Serialize>(
        &self,
        method: &str,
        path: &str,
        body: Option<&B>,
    ) -> Result<ureq::http::Response<ureq::Body>, ApiError> {
        send(method, &self.url(path), body, RPC_TIMEOUT)
    }
}

/// `GET /internal/health` on `http://{ip}:{port}`. HTTP 200 and `status=ok`.
pub fn check_health(ip: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let url = format!("http://{ip}:{port}/internal/health");
    let response = send("GET", &url, None::<&()>, timeout).map_err(|err| err.to_string())?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("HTTP {status} from {url}"));
    }
    let body: HealthBody = read_json(response).map_err(|err| err.to_string())?;
    if body.status != "ok" {
        return Err(format!("unexpected health body from {url}"));
    }
    Ok(())
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

fn send<B: Serialize>(
    method: &str,
    url: &str,
    body: Option<&B>,
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>, ApiError> {
    let agent = agent(timeout);
    let result = match (method, body) {
        ("GET", _) => agent.get(url).call(),
        ("DELETE", _) => agent.delete(url).call(),
        ("POST", Some(body)) => {
            let bytes = serde_json::to_vec(body)
                .map_err(|err| ApiError::internal(format!("encode JSON: {err}")))?;
            agent
                .post(url)
                .header("Content-Type", "application/json")
                .send(bytes)
        }
        ("POST", None) => agent.post(url).send_empty(),
        ("PUT", Some(body)) => {
            let bytes = serde_json::to_vec(body)
                .map_err(|err| ApiError::internal(format!("encode JSON: {err}")))?;
            agent
                .put(url)
                .header("Content-Type", "application/json")
                .send(bytes)
        }
        (other, _) => {
            return Err(ApiError::internal(format!(
                "unsupported internal HTTP method: {other}"
            )));
        }
    };
    result.map_err(|err| ApiError::internal(err.to_string()))
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
        let body: CreateSimulationResponse = read_json(self.send_json(
            "POST",
            "/internal/simulations",
            Some(&CreateSimulationRequest { topology }),
        )?)?;
        Ok(body.id)
    }

    fn start(&self, id: &str, until_ns: Option<u64>) -> Result<(), ApiError> {
        drop_body(self.send_json(
            "POST",
            &format!("/internal/simulations/{id}/start"),
            Some(&StartSimulationRequest { until_ns }),
        )?)
    }

    fn stop(&self, id: &str) -> Result<(), ApiError> {
        drop_body(self.send_json(
            "POST",
            &format!("/internal/simulations/{id}/stop"),
            None::<&()>,
        )?)
    }

    fn reset(&self, id: &str) -> Result<(), ApiError> {
        drop_body(self.send_json(
            "POST",
            &format!("/internal/simulations/{id}/reset"),
            None::<&()>,
        )?)
    }

    fn delete(&self, id: &str) -> Result<(), ApiError> {
        drop_body(self.send_json(
            "DELETE",
            &format!("/internal/simulations/{id}"),
            None::<&()>,
        )?)
    }

    fn exists(&self, id: &str) -> Result<bool, ApiError> {
        Ok(self
            .get_json::<ExistsBody>(&format!("/internal/simulations/{id}/exists"))?
            .exists)
    }

    fn status(&self, id: &str) -> Result<StatusEvent, ApiError> {
        self.get_json(&format!("/internal/simulations/{id}/status"))
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
        drop_body(self.send_json(
            "POST",
            &format!("/internal/simulations/{id}/actions"),
            Some(&request),
        )?)
    }

    fn probe_catalog(&self, id: &str) -> Result<Vec<ProbeCatalogItem>, ApiError> {
        self.get_json(&format!("/internal/simulations/{id}/probes/catalog"))
    }

    fn probe_watch_get(&self, id: &str) -> Result<ProbeWatchView, ApiError> {
        self.get_json(&format!("/internal/simulations/{id}/probes/watch"))
    }

    fn probe_watch_put(&self, id: &str, probes: Vec<ProbeRef>) -> Result<ProbeWatchView, ApiError> {
        read_json(self.send_json(
            "PUT",
            &format!("/internal/simulations/{id}/probes/watch"),
            Some(&probes),
        )?)
    }

    fn probe_watch_clear(&self, id: &str) -> Result<(), ApiError> {
        drop_body(self.send_json(
            "DELETE",
            &format!("/internal/simulations/{id}/probes/watch"),
            None::<&()>,
        )?)
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
        let body: CsvStartBody = read_json(self.send_json(
            "POST",
            &format!("/internal/simulations/{id}/probes/csv/start"),
            Some(&range),
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
        let body: ViewerTokenBody = read_json(self.send_json(
            "POST",
            &format!("/internal/simulations/{id}/viewer/attach"),
            None::<&()>,
        )?)?;
        Ok(body.token)
    }

    fn viewer_check(&self, id: &str, token: &str) -> Result<(), ApiError> {
        drop_body(self.send_json(
            "POST",
            &format!("/internal/simulations/{id}/viewer/check"),
            Some(&ViewerTokenBody {
                token: token.to_string(),
            }),
        )?)
    }

    fn viewer_detach(&self, id: &str, token: &str) -> Result<(), ApiError> {
        drop_body(self.send_json(
            "POST",
            &format!("/internal/simulations/{id}/viewer/detach"),
            Some(&ViewerTokenBody {
                token: token.to_string(),
            }),
        )?)
    }

    fn inspect(
        &self,
        id: &str,
        board_id: &str,
        req: InspectRequest,
    ) -> Result<InspectReply, ApiError> {
        read_json(self.send_json(
            "POST",
            &format!("/internal/simulations/{id}/boards/{board_id}/inspect"),
            Some(&req),
        )?)
    }

    fn step(&self, id: &str, board_id: &str) -> Result<(), ApiError> {
        drop_body(self.send_json(
            "POST",
            &format!("/internal/simulations/{id}/boards/{board_id}/step"),
            None::<&()>,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::backend::serve::router;
    use crate::contract::ActionList;
    use crate::host::SessionNode;
    use crate::probe_log::{ProbeKind, ProbePointName, ProbeRecord};

    use super::*;

    struct MockBackend {
        last: Mutex<String>,
    }

    impl MockBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                last: Mutex::new(String::new()),
            })
        }

        fn set(&self, op: &str) {
            *self.last.lock().unwrap() = op.to_string();
        }
    }

    impl SimulationBackend for MockBackend {
        fn create(&self, _topology: LogicalTopology) -> Result<String, ApiError> {
            self.set("create");
            Ok("sim-1".into())
        }
        fn start(&self, id: &str, until_ns: Option<u64>) -> Result<(), ApiError> {
            self.set(&format!("start:{id}:{}", until_ns.unwrap_or(0)));
            Ok(())
        }
        fn stop(&self, id: &str) -> Result<(), ApiError> {
            self.set(&format!("stop:{id}"));
            Ok(())
        }
        fn reset(&self, id: &str) -> Result<(), ApiError> {
            self.set(&format!("reset:{id}"));
            Ok(())
        }
        fn delete(&self, id: &str) -> Result<(), ApiError> {
            self.set(&format!("delete:{id}"));
            Ok(())
        }
        fn exists(&self, id: &str) -> Result<bool, ApiError> {
            Ok(id == "sim-1")
        }
        fn status(&self, id: &str) -> Result<StatusEvent, ApiError> {
            if id != "sim-1" {
                return Err(ApiError::not_found("unknown simulation"));
            }
            Ok(StatusEvent {
                id: id.into(),
                state: "stopped".into(),
                virtual_time_ns: 0,
                stop_reason: None,
                nodes: vec![SessionNode {
                    id: "mcu-a".into(),
                    kind: "rl78".into(),
                    state: "stopped".into(),
                }],
            })
        }
        fn logs_after(&self, _id: &str, _after: u64) -> Result<Vec<LogEvent>, ApiError> {
            Ok(vec![])
        }
        fn log_high_water(&self, _id: &str) -> Result<u64, ApiError> {
            Ok(7)
        }
        fn actions(&self, _id: &str) -> Result<ActionList, ApiError> {
            Ok(ActionList { actions: vec![] })
        }
        fn invoke_action(&self, _id: &str, _request: InvokeActionRequest) -> Result<(), ApiError> {
            Ok(())
        }
        fn probe_catalog(&self, _id: &str) -> Result<Vec<ProbeCatalogItem>, ApiError> {
            Ok(vec![])
        }
        fn probe_watch_get(&self, _id: &str) -> Result<ProbeWatchView, ApiError> {
            Ok(ProbeWatchView { probes: vec![] })
        }
        fn probe_watch_put(
            &self,
            _id: &str,
            probes: Vec<ProbeRef>,
        ) -> Result<ProbeWatchView, ApiError> {
            Ok(ProbeWatchView { probes })
        }
        fn probe_watch_clear(&self, _id: &str) -> Result<(), ApiError> {
            Ok(())
        }
        fn probe_watch_live(&self, _id: &str, _after: u64) -> Result<Vec<CoarseBatch>, ApiError> {
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
    fn tcpip_round_trip_and_error() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base, _join) = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = router(MockBackend::new());
            let join = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            (format!("http://{addr}"), join)
        });

        let backend = TcpipBackend::new(&base);
        check_health(
            "127.0.0.1",
            url::Url::parse(&base).unwrap().port().unwrap(),
            Duration::from_secs(2),
        )
        .unwrap();

        let id = backend.create(one_board()).unwrap();
        assert_eq!(id, "sim-1");
        backend.start(&id, Some(10)).unwrap();
        backend.stop(&id).unwrap();
        backend.reset(&id).unwrap();
        assert!(backend.exists(&id).unwrap());
        assert_eq!(backend.status(&id).unwrap().id, "sim-1");
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
        backend.delete(&id).unwrap();

        let missing = backend.status("missing").unwrap_err();
        assert_eq!(missing.status, 404);
        assert_eq!(missing.code, "not_found");
    }
}
