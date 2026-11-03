use std::sync::Arc;
use std::time::Duration;

use crate::backend::SimulationBackend;
use crate::contract::{
    ActionList, ApiError, InvokeActionRequest, LogEvent, ProbeSamplesResponse, StatusEvent,
    TopologyView,
};
use crate::host::SessionNode;
use crate::inspect::{InspectReply, InspectRequest};
use crate::probe_log::{
    CoarseBatch, CsvPoll, ProbeCatalogItem, ProbePointName, ProbeRange, ProbeRecord, ProbeRef,
    ProbeWatchView,
};
use crate::topology::LogicalTopology;

use super::router;

struct TopoMock;

impl SimulationBackend for TopoMock {
    fn create(&self, _topology: LogicalTopology) -> Result<String, ApiError> {
        Ok("sim-1".into())
    }
    fn start(&self, _id: &str, _until_ns: Option<u64>) -> Result<(), ApiError> {
        Ok(())
    }
    fn stop(&self, _id: &str) -> Result<(), ApiError> {
        Ok(())
    }
    fn reset(&self, _id: &str) -> Result<(), ApiError> {
        Ok(())
    }
    fn delete(&self, _id: &str) -> Result<(), ApiError> {
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
    fn topology(&self, id: &str) -> Result<TopologyView, ApiError> {
        if id != "sim-1" {
            return Err(ApiError::not_found("unknown simulation"));
        }
        Ok(TopologyView::from_logical(&LogicalTopology {
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
        }))
    }
    fn logs_after(&self, _id: &str, _after: u64) -> Result<Vec<LogEvent>, ApiError> {
        Ok(vec![])
    }
    fn log_high_water(&self, _id: &str) -> Result<u64, ApiError> {
        Ok(0)
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
        Ok((0, vec![]))
    }
    fn csv_poll(&self, _id: &str, _request_id: u64) -> Result<CsvPoll, ApiError> {
        Ok(CsvPoll {
            records: Vec::<ProbeRecord>::new(),
            done: true,
            failed: false,
            truncated: false,
        })
    }
    fn csv_status(&self, id: &str, request_id: u64) -> Result<CsvPoll, ApiError> {
        self.csv_poll(id, request_id)
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

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn spawn_frontend() -> (String, tempfile::TempDir) {
    let web = tempfile::tempdir().unwrap();
    std::fs::write(web.path().join("index.html"), "<!doctype html>").unwrap();
    std::fs::create_dir_all(web.path().join("assets")).unwrap();

    let rt = tokio::runtime::Handle::current();
    let web_root = web.path().to_path_buf();
    let (base_tx, base_rx) = std::sync::mpsc::channel();
    rt.spawn(async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        base_tx.send(format!("http://{addr}")).unwrap();
        let app = router(Arc::new(TopoMock), "127.0.0.1", web_root);
        axum::serve(listener, app).await.unwrap();
    });
    let base = base_rx.recv().unwrap();
    (base, web)
}

#[test]
fn simulation_topology_requires_viewer_token() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (base, _web) = spawn_frontend();
        let url = format!("{base}/api/simulation/sim-1/topology");
        let res = agent().get(&url).call().unwrap();
        assert_eq!(res.status().as_u16(), 409);
    });
}

#[test]
fn simulation_topology_returns_boards_without_elf() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (base, _web) = spawn_frontend();
        let url = format!("{base}/api/simulation/sim-1/topology");
        let mut res = agent()
            .get(&url)
            .header("x-viewer-token", "tok")
            .call()
            .unwrap();
        assert_eq!(res.status().as_u16(), 200);
        let bytes = res.body_mut().read_to_vec().unwrap();
        let view: TopologyView = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(view.boards.len(), 1);
        assert_eq!(view.boards[0].id, "mcu-a");
        assert_eq!(view.boards[0].kind, "rl78");
        assert!(view.edges.is_empty());
        let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(raw["boards"][0].get("elf").is_none());
    });
}
