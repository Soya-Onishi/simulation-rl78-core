//! Probe backend health for assignment candidates.

use std::time::Duration;

use crate::backend::fetch_health;
use crate::config::ServerAddr;

use super::{BackendCapacity, capacity_from_health};

/// Result of probing one configured server.
#[derive(Clone, Debug)]
pub enum ServerProbe {
    Healthy(BackendCapacity),
    Failed { server: ServerAddr, error: String },
}

/// Probe every server. Unhealthy entries are retained as [`ServerProbe::Failed`]
/// and excluded from [`healthy_capacities`].
///
/// [`MultiBackend`](super::MultiBackend) calls this on every create.
pub fn probe_servers(servers: &[ServerAddr], timeout: Duration) -> Vec<ServerProbe> {
    servers
        .iter()
        .map(
            |server| match fetch_health(&server.ip, server.port, timeout) {
                Ok(body) => {
                    ServerProbe::Healthy(capacity_from_health(&server.ip, server.port, &body))
                }
                Err(error) => ServerProbe::Failed {
                    server: server.clone(),
                    error,
                },
            },
        )
        .collect()
}

/// Capacities from successful probes only.
#[must_use]
pub fn healthy_capacities(probes: &[ServerProbe]) -> Vec<BackendCapacity> {
    probes
        .iter()
        .filter_map(|p| match p {
            ServerProbe::Healthy(cap) => Some(cap.clone()),
            ServerProbe::Failed { .. } => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener as StdTcpListener;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    use crate::backend::SimulationBackend;
    use crate::backend::serve::router;
    use crate::contract::{
        ActionList, ApiError, InvokeActionRequest, LogEvent, ProbeSamplesResponse, StatusEvent,
        TopologyView,
    };
    use crate::inspect::{InspectReply, InspectRequest};
    use crate::probe_log::{
        CoarseBatch, CsvPoll, ProbeCatalogItem, ProbePointName, ProbeRange, ProbeRef,
        ProbeWatchView,
    };
    use crate::topology::LogicalTopology;

    use super::super::ClusterSettings;
    use super::*;

    /// Backend whose operations are never called: only `/internal/health` is used.
    struct HealthOnlyBackend;

    fn unused<T>() -> Result<T, ApiError> {
        Err(ApiError::internal("unused in capacity tests"))
    }

    impl SimulationBackend for HealthOnlyBackend {
        fn create(&self, _topology: LogicalTopology) -> Result<String, ApiError> {
            unused()
        }
        fn start(&self, _id: &str, _until_ns: Option<u64>) -> Result<(), ApiError> {
            unused()
        }
        fn stop(&self, _id: &str) -> Result<(), ApiError> {
            unused()
        }
        fn reset(&self, _id: &str) -> Result<(), ApiError> {
            unused()
        }
        fn shutdown(&self, _id: &str) -> Result<(), ApiError> {
            unused()
        }
        fn exists(&self, _id: &str) -> Result<bool, ApiError> {
            unused()
        }
        fn status(&self, _id: &str) -> Result<StatusEvent, ApiError> {
            unused()
        }
        fn topology(&self, _id: &str) -> Result<TopologyView, ApiError> {
            unused()
        }
        fn logs_after(&self, _id: &str, _after: u64) -> Result<Vec<LogEvent>, ApiError> {
            unused()
        }
        fn log_high_water(&self, _id: &str) -> Result<u64, ApiError> {
            unused()
        }
        fn actions(&self, _id: &str) -> Result<ActionList, ApiError> {
            unused()
        }
        fn invoke_action(&self, _id: &str, _request: InvokeActionRequest) -> Result<(), ApiError> {
            unused()
        }
        fn probe_catalog(&self, _id: &str) -> Result<Vec<ProbeCatalogItem>, ApiError> {
            unused()
        }
        fn probe_watch_get(&self, _id: &str) -> Result<ProbeWatchView, ApiError> {
            unused()
        }
        fn probe_watch_put(
            &self,
            _id: &str,
            _probes: Vec<ProbeRef>,
        ) -> Result<ProbeWatchView, ApiError> {
            unused()
        }
        fn probe_watch_clear(&self, _id: &str) -> Result<(), ApiError> {
            unused()
        }
        fn probe_watch_live(&self, _id: &str, _after: u64) -> Result<Vec<CoarseBatch>, ApiError> {
            unused()
        }
        fn csv_start(
            &self,
            _id: &str,
            _range: ProbeRange,
        ) -> Result<(u64, Vec<ProbePointName>), ApiError> {
            unused()
        }
        fn csv_poll(&self, _id: &str, _request_id: u64) -> Result<CsvPoll, ApiError> {
            unused()
        }
        fn csv_status(&self, _id: &str, _request_id: u64) -> Result<CsvPoll, ApiError> {
            unused()
        }
        fn probe_samples(
            &self,
            _id: &str,
            _board: &str,
            _probe: &str,
            _from_ns: Option<u64>,
            _to_ns: Option<u64>,
        ) -> Result<ProbeSamplesResponse, ApiError> {
            unused()
        }
        fn viewer_attach(&self, _id: &str) -> Result<String, ApiError> {
            unused()
        }
        fn viewer_check(&self, _id: &str, _token: &str) -> Result<(), ApiError> {
            unused()
        }
        fn viewer_detach(&self, _id: &str, _token: &str) -> Result<(), ApiError> {
            unused()
        }
        fn inspect(
            &self,
            _id: &str,
            _board_id: &str,
            _req: InspectRequest,
        ) -> Result<InspectReply, ApiError> {
            unused()
        }
        fn step(&self, _id: &str, _board_id: &str) -> Result<(), ApiError> {
            unused()
        }
    }

    fn cap(backend: &str, max_boards: u32, running_boards: u32) -> BackendCapacity {
        BackendCapacity {
            backend: backend.into(),
            max_boards,
            running_boards,
            settings: ClusterSettings {
                time_sync_period_ms: 100,
                data_udp_window: 32,
                multicast_base: "239.255.0.0".into(),
            },
        }
    }

    #[test]
    fn healthy_capacities_filters_failed_and_keeps_healthy() {
        let probes = vec![
            ServerProbe::Healthy(cap("10.0.0.1:8091", 8, 2)),
            ServerProbe::Failed {
                server: ServerAddr {
                    ip: "10.0.0.2".into(),
                    port: 8091,
                },
                error: "connection refused".into(),
            },
            ServerProbe::Healthy(cap("10.0.0.3:8091", 4, 4)),
        ];
        let caps = healthy_capacities(&probes);
        assert_eq!(caps.len(), 2);
        assert_eq!(caps[0].backend, "10.0.0.1:8091");
        assert_eq!(caps[0].max_boards, 8);
        assert_eq!(caps[0].running_boards, 2);
        assert_eq!(caps[1].backend, "10.0.0.3:8091");

        assert!(healthy_capacities(&[]).is_empty());
        let only_failed = vec![ServerProbe::Failed {
            server: ServerAddr {
                ip: "10.0.0.2".into(),
                port: 8091,
            },
            error: "x".into(),
        }];
        assert!(healthy_capacities(&only_failed).is_empty());
    }

    #[test]
    fn probe_servers_marks_dead_port_failed_and_live_backend_healthy() {
        // Reserve a port, then drop the listener so nothing is listening there.
        let dead_port = {
            let l = StdTcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let live_port = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let running = Arc::new(AtomicU32::new(3));
            let app = router(
                Arc::new(HealthOnlyBackend),
                crate::backend::BackendInfo::for_tests(16),
                running,
            );
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            port
        });

        let servers = vec![
            ServerAddr {
                ip: "127.0.0.1".into(),
                port: dead_port,
            },
            ServerAddr {
                ip: "127.0.0.1".into(),
                port: live_port,
            },
        ];
        let probes = probe_servers(&servers, Duration::from_secs(2));
        assert_eq!(probes.len(), 2);

        match &probes[0] {
            ServerProbe::Failed { server, error } => {
                assert_eq!(server.port, dead_port);
                assert!(!error.is_empty());
            }
            ServerProbe::Healthy(c) => panic!("dead port must fail, got {c:?}"),
        }
        match &probes[1] {
            ServerProbe::Healthy(c) => {
                assert_eq!(c.backend, format!("127.0.0.1:{live_port}"));
                assert_eq!(c.max_boards, 16);
                assert_eq!(c.running_boards, 3);
                assert_eq!(c.settings.time_sync_period_ms, 100);
                assert_eq!(c.settings.data_udp_window, 32);
                assert_eq!(c.settings.multicast_base, "239.255.0.0");
            }
            ServerProbe::Failed { error, .. } => panic!("live backend must be healthy: {error}"),
        }

        let caps = healthy_capacities(&probes);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].backend, format!("127.0.0.1:{live_port}"));
    }
}
