//! Log pub/sub: nodes → arbiter (`log/n2a`) and arbiter → server (`log/a2s`).
//!
//! The server creates `log/a2s` before spawning the arbiter. The arbiter creates
//! `log/n2a` and opens `log/a2s` as the publisher. The arbiter control thread only
//! enqueues its own records; a dedicated thread forwards those and node samples.
//! QoS on create and `open_or_create` must stay identical.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Once, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::log_msg::{ClusterLog, LogConsole, LogLevel, LogOrigin};

use super::names;
use super::runtime::{IpcError, create_node, isolated_config};

type N2aLogPub = Publisher<ipc::Service, ClusterLog, ()>;
type N2aLogSub = Subscriber<ipc::Service, ClusterLog, ()>;
type A2sLogPub = Publisher<ipc::Service, ClusterLog, ()>;
type A2sLogSub = Subscriber<ipc::Service, ClusterLog, ()>;

/// Node-side guard. Installing it makes [`log`] macros in this process enqueue
/// node-originated records; a thread publishes them on `log/n2a`.
pub struct NodeLog {
    queue: Arc<LogQueue>,
    worker: Option<JoinHandle<()>>,
}

impl NodeLog {
    pub fn start(
        iox_root: &Path,
        cluster_key: &str,
        board_hash: u64,
        board_count: usize,
    ) -> Result<Self, IpcError> {
        let queue = Arc::new(LogQueue::new(LogOrigin::Node, board_hash));
        install_process_log(Arc::clone(&queue));
        let (ready_tx, ready_rx) = mpsc::channel();
        let queue_for_worker = Arc::clone(&queue);
        let iox_root = iox_root.to_path_buf();
        let cluster_key = cluster_key.to_string();
        let worker = match thread::Builder::new()
            .name("node-log".into())
            .spawn(move || {
                publish_node_logs(
                    iox_root,
                    cluster_key,
                    board_hash,
                    board_count,
                    queue_for_worker,
                    ready_tx,
                );
            }) {
            Ok(worker) => worker,
            Err(err) => return Err(IpcError::Message(format!("node log thread: {err}"))),
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                queue,
                worker: Some(worker),
            }),
            Ok(Err(err)) => {
                let _ = worker.join();
                Err(IpcError::Message(err))
            }
            Err(_) => {
                let _ = worker.join();
                Err(IpcError::Message(
                    "node log thread exited before the publisher was ready".into(),
                ))
            }
        }
    }

    fn shutdown(&mut self) {
        self.queue.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for NodeLog {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// How many records a process may queue before the forwarder drains them.
const LOG_QUEUE_CAP: usize = 256;

/// Poll interval of the forwarder when both the queue and the node bus are empty.
const LOG_WORKER_IDLE: Duration = Duration::from_millis(1);

struct LogQueue {
    records: Mutex<VecDeque<ClusterLog>>,
    shutdown: AtomicBool,
    origin: LogOrigin,
    board_hash: u64,
}

impl LogQueue {
    fn new(origin: LogOrigin, board_hash: u64) -> Self {
        Self {
            records: Mutex::new(VecDeque::new()),
            shutdown: AtomicBool::new(false),
            origin,
            board_hash,
        }
    }

    fn push_text(&self, level: LogLevel, text: &str) {
        self.push(ClusterLog::record(
            level,
            self.origin,
            self.board_hash,
            text,
        ));
    }

    fn push(&self, record: ClusterLog) {
        let mut records = self.records.lock().unwrap_or_else(|err| err.into_inner());
        if records.len() >= LOG_QUEUE_CAP {
            records.pop_front();
        }
        records.push_back(record);
    }

    fn take_all(&self) -> VecDeque<ClusterLog> {
        let mut records = self.records.lock().unwrap_or_else(|err| err.into_inner());
        std::mem::take(&mut *records)
    }
}

/// Arbiter side: the control thread enqueues; the forwarder thread publishes.
pub struct ArbiterLogBus {
    queue: Arc<LogQueue>,
    worker: Option<JoinHandle<()>>,
}

impl ArbiterLogBus {
    /// Start the forwarder. It owns its own iceoryx node because pub/sub ports are
    /// single-threaded. Returns after that node has opened the log services.
    pub fn create(
        iox_root: &Path,
        cluster_key: &str,
        board_count: usize,
        boards: HashMap<u64, String>,
    ) -> Result<Self, IpcError> {
        let queue = Arc::new(LogQueue::new(LogOrigin::Arbiter, 0));
        install_process_log(Arc::clone(&queue));
        let (ready_tx, ready_rx) = mpsc::channel();
        let queue_for_worker = Arc::clone(&queue);
        let iox_root = iox_root.to_path_buf();
        let cluster_key = cluster_key.to_string();
        let worker = match thread::Builder::new()
            .name("arbiter-log".into())
            .spawn(move || {
                forward_logs(
                    iox_root,
                    cluster_key,
                    board_count,
                    boards,
                    queue_for_worker,
                    ready_tx,
                );
            }) {
            Ok(worker) => worker,
            Err(err) => {
                return Err(IpcError::Message(format!("log forwarder thread: {err}")));
            }
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                queue,
                worker: Some(worker),
            }),
            Ok(Err(err)) => {
                let _ = worker.join();
                Err(IpcError::Message(err))
            }
            Err(_) => {
                let _ = worker.join();
                Err(IpcError::Message(
                    "log forwarder exited before the log ports were ready".into(),
                ))
            }
        }
    }

    /// Flush queued records and join the forwarder.
    pub fn shutdown(&mut self) {
        self.queue.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ArbiterLogBus {
    fn drop(&mut self) {
        self.shutdown();
    }
}

static PROCESS_LOG: RwLock<Option<Arc<LogQueue>>> = RwLock::new(None);

struct ProcessLog;

impl log::Log for ProcessLog {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let Some(queue) = PROCESS_LOG
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
        else {
            return;
        };
        queue.push_text(level_from_log(record.level()), &record.args().to_string());
    }

    fn flush(&self) {}
}

fn level_from_log(level: log::Level) -> LogLevel {
    match level {
        log::Level::Error => LogLevel::Error,
        log::Level::Warn => LogLevel::Warn,
        log::Level::Info => LogLevel::Info,
        log::Level::Debug => LogLevel::Debug,
        log::Level::Trace => LogLevel::Trace,
    }
}

fn install_process_log(queue: Arc<LogQueue>) {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = log::set_logger(&ProcessLog);
        log::set_max_level(log::LevelFilter::Trace);
    });

    *PROCESS_LOG.write().unwrap_or_else(|err| err.into_inner()) = Some(queue);
}

fn publish_node_logs(
    iox_root: PathBuf,
    cluster_key: String,
    board_hash: u64,
    board_count: usize,
    queue: Arc<LogQueue>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    let opened: Result<_, IpcError> = (|| {
        let config = isolated_config(&iox_root)?;
        let node = create_node(&config, &format!("nlog-{board_hash}"))?;
        let publisher = open_or_create_n2a_publisher(&node, &cluster_key, board_count)?;
        Ok((node, publisher))
    })();
    let (node, publisher) = match opened {
        Ok(ports) => {
            let _ = ready.send(Ok(()));
            ports
        }
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };
    let _node = node;
    let console = LogConsole::new(LogLevel::Trace);
    loop {
        for record in queue.take_all() {
            if publisher.send_copy(record).is_err() {
                console.accept(&record, |_| None);
            }
        }
        if queue.shutdown.load(Ordering::Acquire) {
            for record in queue.take_all() {
                if publisher.send_copy(record).is_err() {
                    console.accept(&record, |_| None);
                }
            }
            break;
        }
        thread::sleep(LOG_WORKER_IDLE);
    }
}

fn forward_logs(
    iox_root: PathBuf,
    cluster_key: String,
    board_count: usize,
    boards: HashMap<u64, String>,
    queue: Arc<LogQueue>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    let opened: Result<_, IpcError> = (|| {
        let config = isolated_config(&iox_root)?;
        let node = create_node(&config, &format!("log-{cluster_key}"))?;
        let n2a_sub = create_n2a_subscriber(&node, &cluster_key, board_count)?;
        let upstream = open_or_create_a2s_publisher(&node, &cluster_key)?;
        Ok((node, n2a_sub, upstream))
    })();
    let (node, n2a_sub, upstream) = match opened {
        Ok(ports) => {
            let _ = ready.send(Ok(()));
            ports
        }
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };
    let _node = node;
    loop {
        forward_node_logs(&n2a_sub, &upstream, &boards);
        publish_all(&upstream, queue.take_all());
        if queue.shutdown.load(Ordering::Acquire) {
            forward_node_logs(&n2a_sub, &upstream, &boards);
            publish_all(&upstream, queue.take_all());
            break;
        }
        thread::sleep(LOG_WORKER_IDLE);
    }
}

fn forward_node_logs(n2a_sub: &N2aLogSub, upstream: &A2sLogPub, boards: &HashMap<u64, String>) {
    loop {
        match n2a_sub.receive() {
            Ok(Some(sample)) => {
                let record: ClusterLog = *sample;
                if record.origin() == LogOrigin::Node && !boards.contains_key(&record.board_hash())
                {
                    continue;
                }
                let _ = upstream.send_copy(record);
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

fn publish_all(upstream: &A2sLogPub, records: VecDeque<ClusterLog>) {
    for record in records {
        let _ = upstream.send_copy(record);
    }
}

/// Server subscriber for arbiter-forwarded [`ClusterLog`] samples.
pub struct ServerLogInbox {
    sub: A2sLogSub,
}

impl ServerLogInbox {
    pub fn create(node: &Node<ipc::Service>, cluster_key: &str) -> Result<Self, IpcError> {
        let name = names::log_a2s(cluster_key);
        let svc_name: ServiceName = name
            .as_str()
            .try_into()
            .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
        let svc = node
            .service_builder(&svc_name)
            .publish_subscribe::<ClusterLog>()
            .max_publishers(1)
            .max_subscribers(1)
            .max_nodes(4)
            .subscriber_max_buffer_size(64)
            .history_size(16)
            .enable_safe_overflow(true)
            .create()
            .map_err(|e| IpcError::Message(format!("create log a2s failed: {e:?}")))?;
        let sub = svc
            .subscriber_builder()
            .create()
            .map_err(|e| IpcError::Message(format!("log a2s subscriber: {e:?}")))?;
        Ok(Self { sub })
    }

    pub fn try_recv(&self) -> Result<Option<ClusterLog>, IpcError> {
        match self
            .sub
            .receive()
            .map_err(|e| IpcError::Message(format!("log a2s receive: {e:?}")))?
        {
            Some(sample) => Ok(Some(*sample)),
            None => Ok(None),
        }
    }

    pub fn drain(
        &self,
        console: &LogConsole,
        boards: &HashMap<u64, String>,
    ) -> Result<(), IpcError> {
        while let Some(record) = self.try_recv()? {
            console.accept(&record, |h| boards.get(&h).map(String::as_str));
        }
        Ok(())
    }
}

fn create_n2a_subscriber(
    node: &Node<ipc::Service>,
    cluster_key: &str,
    board_count: usize,
) -> Result<N2aLogSub, IpcError> {
    let max_nodes = board_count.max(1);
    let name = names::log_n2a(cluster_key);
    let svc_name: ServiceName = name
        .as_str()
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<ClusterLog>()
        .max_publishers(max_nodes)
        .max_subscribers(1)
        .max_nodes(max_nodes + 4)
        .subscriber_max_buffer_size(64)
        .history_size(16)
        .enable_safe_overflow(true)
        .create()
        .map_err(|e| IpcError::Message(format!("create log n2a failed: {e:?}")))?;
    svc.subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("log n2a subscriber: {e:?}")))
}

fn open_or_create_n2a_publisher(
    node: &Node<ipc::Service>,
    cluster_key: &str,
    board_count: usize,
) -> Result<N2aLogPub, IpcError> {
    let max_nodes = board_count.max(1);
    let name = names::log_n2a(cluster_key);
    let svc_name: ServiceName = name
        .as_str()
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<ClusterLog>()
        .max_publishers(max_nodes)
        .max_subscribers(1)
        .max_nodes(max_nodes + 4)
        .subscriber_max_buffer_size(64)
        .history_size(16)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| {
            IpcError::Message(format!("open_or_create log n2a publisher {name}: {e:?}"))
        })?;
    svc.publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("log n2a publisher {name}: {e:?}")))
}

fn open_or_create_a2s_publisher(
    node: &Node<ipc::Service>,
    cluster_key: &str,
) -> Result<A2sLogPub, IpcError> {
    let name = names::log_a2s(cluster_key);
    let svc_name: ServiceName = name
        .as_str()
        .try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))?;
    let svc = node
        .service_builder(&svc_name)
        .publish_subscribe::<ClusterLog>()
        .max_publishers(1)
        .max_subscribers(1)
        .max_nodes(4)
        .subscriber_max_buffer_size(64)
        .history_size(16)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| {
            IpcError::Message(format!("open_or_create log a2s publisher {name}: {e:?}"))
        })?;
    svc.publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("log a2s publisher {name}: {e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::hash::{board_hash_table, board_id_hash};
    use crate::ipc::runtime::{create_node, isolated_config};
    use serial_test::serial;
    use std::time::Duration;

    #[test]
    #[serial]
    fn node_log_is_forwarded_to_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let key = format!("log-rt-{}", std::process::id());
        let root = dir.path().join("iox");
        let boards = board_hash_table(["mcu0"]).unwrap();
        let config = isolated_config(&root).unwrap();

        let server_node = create_node(&config, "log-server").unwrap();
        let inbox = ServerLogInbox::create(&server_node, &key).unwrap();

        let _bus = ArbiterLogBus::create(&root, &key, 1, boards).unwrap();
        log::info!("Start broadcast");

        let hash = board_id_hash("mcu0");
        let _node_log = NodeLog::start(&root, &key, hash, 1).unwrap();
        log::info!("Ready");

        let mut got_node = false;
        let mut got_arbiter = false;
        for _ in 0..200 {
            if let Some(record) = inbox.try_recv().unwrap() {
                match record.origin() {
                    LogOrigin::Node => {
                        assert_eq!(record.board_hash(), hash);
                        assert_eq!(record.level(), LogLevel::Info);
                        assert_eq!(record.text(), "Ready");
                        got_node = true;
                    }
                    LogOrigin::Arbiter => {
                        assert_eq!(record.text(), "Start broadcast");
                        got_arbiter = true;
                    }
                }
            }
            if got_node && got_arbiter {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(got_node, "server did not receive the node log");
        assert!(got_arbiter, "server did not receive the arbiter log");
    }
}
