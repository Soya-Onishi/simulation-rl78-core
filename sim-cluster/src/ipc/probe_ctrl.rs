//! Server ↔ arbiter ↔ node probe watch-set and query/reply channels.

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;

use crate::ipc::HashedId;
use crate::probe::MAX_PROBES_PER_BOARD;
use crate::probe_log::{ProbeKind, ProbeRecord};

use super::names;
use super::runtime::IpcError;

/// Samples packed into one query-reply message.
pub const QUERY_CHUNK: usize = 32;

/// CSV chunks a node may publish in one board-loop turn.
pub const CSV_CHUNKS_PER_TICK: usize = 4;

pub const QUERY_KIND_LATEST: u8 = 1;
pub const QUERY_KIND_RANGE: u8 = 2;
pub const QUERY_REPLY_LAST: u8 = 1;
pub const QUERY_REPLY_TRUNCATED: u8 = 2;

/// Subscriber buffer for query/CSV reply channels.
/// Query replies disable safe overflow and block when this is full.
const QUERY_BUFFER: usize = 1024;
const WATCH_BUFFER: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub struct ProbeWatchWire {
    pub board_hash: HashedId<u64>,
    pub count: u8,
    pub _pad: [u8; 7],
    pub probe_ids: [HashedId<u64>; MAX_PROBES_PER_BOARD],
}

impl ProbeWatchWire {
    #[must_use]
    pub fn new(board_hash: HashedId<u64>, ids: &[HashedId<u64>]) -> Self {
        let mut probe_ids = [HashedId::new(0); MAX_PROBES_PER_BOARD];
        let n = ids.len().min(MAX_PROBES_PER_BOARD);
        probe_ids[..n].copy_from_slice(&ids[..n]);
        Self {
            board_hash,
            count: u8::try_from(n).unwrap_or(u8::MAX),
            _pad: [0; 7],
            probe_ids,
        }
    }

    #[must_use]
    pub fn ids(self) -> [HashedId<u64>; MAX_PROBES_PER_BOARD] {
        let mut ids = self.probe_ids;
        let n = usize::from(self.count).min(MAX_PROBES_PER_BOARD);
        for slot in ids.iter_mut().skip(n) {
            *slot = HashedId::new(0);
        }
        ids
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub struct ProbeQueryWire {
    pub request_id: u64,
    pub board_hash: HashedId<u64>,
    pub from_ns: u64,
    pub to_ns: u64,
    pub kind: u8,
    pub count: u8,
    pub has_from: u8,
    pub has_to: u8,
    pub _pad: [u8; 4],
    pub probe_ids: [HashedId<u64>; MAX_PROBES_PER_BOARD],
}

impl ProbeQueryWire {
    #[must_use]
    pub fn latest(request_id: u64, board_hash: HashedId<u64>, ids: &[HashedId<u64>]) -> Self {
        let mut probe_ids = [HashedId::new(0); MAX_PROBES_PER_BOARD];
        let n = ids.len().min(MAX_PROBES_PER_BOARD);
        probe_ids[..n].copy_from_slice(&ids[..n]);
        Self {
            request_id,
            board_hash,
            from_ns: 0,
            to_ns: 0,
            kind: QUERY_KIND_LATEST,
            count: u8::try_from(n).unwrap_or(u8::MAX),
            has_from: 0,
            has_to: 0,
            _pad: [0; 4],
            probe_ids,
        }
    }

    /// Range query on the query channel (JSON samples) or the CSV channel.
    #[must_use]
    pub fn range(
        request_id: u64,
        board_hash: HashedId<u64>,
        ids: &[HashedId<u64>],
        from_ns: Option<u64>,
        to_ns: Option<u64>,
    ) -> Self {
        Self::csv(request_id, board_hash, ids, from_ns, to_ns)
    }

    #[must_use]
    pub fn csv(
        request_id: u64,
        board_hash: HashedId<u64>,
        ids: &[HashedId<u64>],
        from_ns: Option<u64>,
        to_ns: Option<u64>,
    ) -> Self {
        let mut probe_ids = [HashedId::new(0); MAX_PROBES_PER_BOARD];
        let n = ids.len().min(MAX_PROBES_PER_BOARD);
        probe_ids[..n].copy_from_slice(&ids[..n]);
        Self {
            request_id,
            board_hash,
            from_ns: from_ns.unwrap_or(0),
            to_ns: to_ns.unwrap_or(0),
            kind: QUERY_KIND_RANGE,
            count: u8::try_from(n).unwrap_or(u8::MAX),
            has_from: u8::from(from_ns.is_some()),
            has_to: u8::from(to_ns.is_some()),
            _pad: [0; 4],
            probe_ids,
        }
    }

    #[must_use]
    pub fn probe_ids(&self) -> &[HashedId<u64>] {
        let n = usize::from(self.count).min(MAX_PROBES_PER_BOARD);
        &self.probe_ids[..n]
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub struct ProbeQueryRecord {
    pub virtual_time_ns: u64,
    pub probe_id: HashedId<u64>,
    pub value: u16,
    pub kind: u8,
    pub _pad: u8,
}

impl ProbeQueryRecord {
    #[must_use]
    pub fn from_record(record: ProbeRecord) -> Self {
        Self {
            virtual_time_ns: record.virtual_time_ns,
            probe_id: record.probe_id,
            value: record.value,
            kind: match record.kind {
                ProbeKind::Digital => 1,
                ProbeKind::Uart => 2,
                ProbeKind::Analog => 3,
            },
            _pad: 0,
        }
    }

    #[must_use]
    pub fn to_record(self) -> Option<ProbeRecord> {
        Some(ProbeRecord {
            probe_id: self.probe_id,
            virtual_time_ns: self.virtual_time_ns,
            kind: ProbeKind::from_wire(self.kind)?,
            value: self.value,
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ZeroCopySend)]
pub struct ProbeQueryReplyWire {
    pub request_id: u64,
    pub from: HashedId<u64>,
    pub flags: u8,
    pub count: u8,
    pub _pad: [u8; 6],
    pub records: [ProbeQueryRecord; QUERY_CHUNK],
}

impl ProbeQueryReplyWire {
    #[must_use]
    pub fn chunk(
        request_id: u64,
        from: HashedId<u64>,
        records: &[ProbeRecord],
        last: bool,
        truncated: bool,
    ) -> Self {
        let mut packed = [ProbeQueryRecord {
            virtual_time_ns: 0,
            probe_id: HashedId::new(0),
            value: 0,
            kind: 0,
            _pad: 0,
        }; QUERY_CHUNK];
        let n = records.len().min(QUERY_CHUNK);
        for (slot, record) in packed.iter_mut().zip(records.iter().take(n)) {
            *slot = ProbeQueryRecord::from_record(*record);
        }
        let mut flags = 0;
        if last {
            flags |= QUERY_REPLY_LAST;
        }
        if truncated {
            flags |= QUERY_REPLY_TRUNCATED;
        }
        Self {
            request_id,
            from,
            flags,
            count: u8::try_from(n).unwrap_or(u8::MAX),
            _pad: [0; 6],
            records: packed,
        }
    }

    #[must_use]
    pub fn is_last(self) -> bool {
        self.flags & QUERY_REPLY_LAST != 0
    }

    #[must_use]
    pub fn is_truncated(self) -> bool {
        self.flags & QUERY_REPLY_TRUNCATED != 0
    }

    pub fn records(&self) -> impl Iterator<Item = ProbeRecord> + '_ {
        self.records
            .iter()
            .take(usize::from(self.count).min(QUERY_CHUNK))
            .filter_map(|rec| rec.to_record())
    }
}

type QueryPub = Publisher<ipc::Service, ProbeQueryWire, ()>;
type QuerySub = Subscriber<ipc::Service, ProbeQueryWire, ()>;
type ReplyPub = Publisher<ipc::Service, ProbeQueryReplyWire, ()>;
type ReplySub = Subscriber<ipc::Service, ProbeQueryReplyWire, ()>;

fn svc_name(name: &str) -> Result<ServiceName, IpcError> {
    name.try_into()
        .map_err(|e| IpcError::Message(format!("bad service name {name}: {e:?}")))
}

fn qos<T: ZeroCopySend + std::fmt::Debug>(
    node: &Node<ipc::Service>,
    name: &str,
    publishers: usize,
    subscribers: usize,
    nodes: usize,
    buffer: usize,
    overflow: bool,
    create: bool,
) -> Result<
    iceoryx2::service::port_factory::publish_subscribe::PortFactory<ipc::Service, T, ()>,
    IpcError,
> {
    let builder = node
        .service_builder(&svc_name(name)?)
        .publish_subscribe::<T>()
        .max_publishers(publishers.max(1))
        .max_subscribers(subscribers.max(1))
        .max_nodes(nodes.max(1))
        .subscriber_max_buffer_size(buffer.max(1))
        .history_size(0)
        .enable_safe_overflow(overflow);
    if create {
        builder
            .create()
            .map_err(|e| IpcError::Message(format!("create {name}: {e:?}")))
    } else {
        builder
            .open_or_create()
            .map_err(|e| IpcError::Message(format!("open {name}: {e:?}")))
    }
}

fn publisher<T: ZeroCopySend + std::fmt::Debug>(
    factory: iceoryx2::service::port_factory::publish_subscribe::PortFactory<ipc::Service, T, ()>,
    name: &str,
) -> Result<Publisher<ipc::Service, T, ()>, IpcError> {
    factory
        .publisher_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("publisher {name}: {e:?}")))
}

fn blocking_publisher<T: ZeroCopySend + std::fmt::Debug>(
    factory: iceoryx2::service::port_factory::publish_subscribe::PortFactory<ipc::Service, T, ()>,
    name: &str,
) -> Result<Publisher<ipc::Service, T, ()>, IpcError> {
    factory
        .publisher_builder()
        .backpressure_strategy(BackpressureStrategy::RetryUntilDelivered)
        .create()
        .map_err(|e| IpcError::Message(format!("publisher {name}: {e:?}")))
}

fn subscriber<T: ZeroCopySend + std::fmt::Debug>(
    factory: iceoryx2::service::port_factory::publish_subscribe::PortFactory<ipc::Service, T, ()>,
    name: &str,
) -> Result<Subscriber<ipc::Service, T, ()>, IpcError> {
    factory
        .subscriber_builder()
        .create()
        .map_err(|e| IpcError::Message(format!("subscriber {name}: {e:?}")))
}

fn recv<T: Copy + std::fmt::Debug + ZeroCopySend>(
    sub: &Subscriber<ipc::Service, T, ()>,
    what: &str,
) -> Result<Option<T>, IpcError> {
    match sub
        .receive()
        .map_err(|e| IpcError::Message(format!("{what} receive: {e:?}")))?
    {
        Some(sample) => Ok(Some(*sample)),
        None => Ok(None),
    }
}

fn node_slots(board_count: usize) -> usize {
    board_count.max(1) + 4
}

/// Arbiter side: server legs via mpsc, node query/csv legs via iceoryx.
pub struct ArbiterProbeCtrl {
    server: super::server_arbiter::ArbiterProbeServer,
    query_a2n: QueryPub,
    query_n2a: ReplySub,
    csv_a2n: QueryPub,
    csv_n2a: ReplySub,
}

impl ArbiterProbeCtrl {
    pub fn open(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_count: usize,
        server: super::server_arbiter::ArbiterProbeServer,
    ) -> Result<Self, IpcError> {
        let slots = node_slots(board_count);
        let query_a2n_name = names::probe_query_a2n(cluster_key);
        let query_n2a_name = names::probe_query_n2a(cluster_key);
        let query_a2n = qos::<ProbeQueryWire>(
            node,
            &query_a2n_name,
            1,
            board_count,
            slots,
            WATCH_BUFFER,
            true,
            true,
        )?;
        let query_n2a = qos::<ProbeQueryReplyWire>(
            node,
            &query_n2a_name,
            board_count,
            1,
            slots,
            QUERY_BUFFER.saturating_mul(board_count.max(1)),
            false,
            true,
        )?;
        let csv_a2n_name = names::probe_csv_a2n(cluster_key);
        let csv_n2a_name = names::probe_csv_n2a(cluster_key);
        let csv_a2n = qos::<ProbeQueryWire>(
            node,
            &csv_a2n_name,
            1,
            board_count,
            slots,
            WATCH_BUFFER,
            true,
            true,
        )?;
        let csv_n2a = qos::<ProbeQueryReplyWire>(
            node,
            &csv_n2a_name,
            board_count,
            1,
            slots,
            QUERY_BUFFER.saturating_mul(board_count.max(1)),
            true,
            true,
        )?;
        Ok(Self {
            server,
            query_a2n: publisher(query_a2n, &query_a2n_name)?,
            query_n2a: subscriber(query_n2a, &query_n2a_name)?,
            csv_a2n: publisher(csv_a2n, &csv_a2n_name)?,
            csv_n2a: subscriber(csv_n2a, &csv_n2a_name)?,
        })
    }

    pub fn try_watch(&self) -> Result<Option<ProbeWatchWire>, IpcError> {
        self.server.try_watch()
    }

    pub fn try_query(&self) -> Result<Option<ProbeQueryWire>, IpcError> {
        self.server.try_query()
    }

    pub fn publish_query_to_nodes(&self, sample: ProbeQueryWire) -> Result<(), IpcError> {
        self.query_a2n
            .send_copy(sample)
            .map_err(|e| IpcError::Message(format!("probe query a2n send: {e:?}")))?;
        Ok(())
    }

    pub fn try_node_reply(&self) -> Result<Option<ProbeQueryReplyWire>, IpcError> {
        recv(&self.query_n2a, "probe query n2a")
    }

    pub fn publish_reply_to_server(&self, sample: ProbeQueryReplyWire) -> Result<(), IpcError> {
        self.server.publish_reply_to_server(sample)
    }

    pub fn try_csv(&self) -> Result<Option<ProbeQueryWire>, IpcError> {
        self.server.try_csv()
    }

    pub fn publish_csv_to_nodes(&self, sample: ProbeQueryWire) -> Result<(), IpcError> {
        self.csv_a2n
            .send_copy(sample)
            .map_err(|e| IpcError::Message(format!("probe csv a2n send: {e:?}")))?;
        Ok(())
    }

    pub fn try_node_csv(&self) -> Result<Option<ProbeQueryReplyWire>, IpcError> {
        recv(&self.csv_n2a, "probe csv n2a")
    }

    pub fn publish_csv_to_server(&self, sample: ProbeQueryReplyWire) -> Result<(), IpcError> {
        self.server.publish_csv_to_server(sample)
    }
}

/// Node side of the query channel.
pub struct NodeProbeQuery {
    query: QuerySub,
    reply: ReplyPub,
}

impl NodeProbeQuery {
    pub fn open(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_count: usize,
    ) -> Result<Self, IpcError> {
        let slots = node_slots(board_count);
        let query_name = names::probe_query_a2n(cluster_key);
        let reply_name = names::probe_query_n2a(cluster_key);
        let query = qos::<ProbeQueryWire>(
            node,
            &query_name,
            1,
            board_count,
            slots,
            WATCH_BUFFER,
            true,
            false,
        )?;
        let reply = qos::<ProbeQueryReplyWire>(
            node,
            &reply_name,
            board_count,
            1,
            slots,
            QUERY_BUFFER.saturating_mul(board_count.max(1)),
            false,
            false,
        )?;
        Ok(Self {
            query: subscriber(query, &query_name)?,
            reply: blocking_publisher(reply, &reply_name)?,
        })
    }

    pub fn try_query(&self) -> Result<Option<ProbeQueryWire>, IpcError> {
        recv(&self.query, "probe query a2n")
    }

    pub fn publish_reply(&self, sample: ProbeQueryReplyWire) -> Result<(), IpcError> {
        self.reply
            .send_copy(sample)
            .map_err(|e| IpcError::Message(format!("probe query n2a send: {e:?}")))?;
        Ok(())
    }
}

/// Node side of the CSV export channel.
pub struct NodeProbeCsv {
    query: QuerySub,
    reply: ReplyPub,
}

impl NodeProbeCsv {
    pub fn open(
        node: &Node<ipc::Service>,
        cluster_key: &str,
        board_count: usize,
    ) -> Result<Self, IpcError> {
        let slots = node_slots(board_count);
        let query_name = names::probe_csv_a2n(cluster_key);
        let reply_name = names::probe_csv_n2a(cluster_key);
        let query = qos::<ProbeQueryWire>(
            node,
            &query_name,
            1,
            board_count,
            slots,
            WATCH_BUFFER,
            true,
            false,
        )?;
        let reply = qos::<ProbeQueryReplyWire>(
            node,
            &reply_name,
            board_count,
            1,
            slots,
            QUERY_BUFFER.saturating_mul(board_count.max(1)),
            true,
            false,
        )?;
        Ok(Self {
            query: subscriber(query, &query_name)?,
            reply: publisher(reply, &reply_name)?,
        })
    }

    pub fn try_query(&self) -> Result<Option<ProbeQueryWire>, IpcError> {
        recv(&self.query, "probe csv a2n")
    }

    pub fn publish_reply(&self, sample: ProbeQueryReplyWire) -> Result<(), IpcError> {
        self.reply
            .send_copy(sample)
            .map_err(|e| IpcError::Message(format!("probe csv n2a send: {e:?}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::HashedId;
    use crate::ipc::runtime::{create_node, isolated_config};
    use crate::ipc::server_arbiter;
    use crate::probe_log::{ProbeKind, ProbeRecord};

    #[test]
    fn watch_and_query_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = format!("pq-{}", std::process::id());
        let config = isolated_config(&dir.path().join("iox")).unwrap();
        let (server_link, arb_link) = server_arbiter::open_pair();
        let server = server_link.probe_ctrl;
        let arb_node = create_node(&config, "pq-arb").unwrap();
        let arb = ArbiterProbeCtrl::open(&arb_node, &key, 1, arb_link.probe_ctrl_server).unwrap();
        let node = create_node(&config, "pq-node").unwrap();
        let node_q = NodeProbeQuery::open(&node, &key, 1).unwrap();

        server
            .publish_watch(ProbeWatchWire::new(
                HashedId::new(9),
                &[HashedId::new(1), HashedId::new(2)],
            ))
            .unwrap();
        let watch = arb.try_watch().unwrap().expect("watch");
        assert_eq!(watch.board_hash, HashedId::new(9));
        assert_eq!(watch.count, 2);

        let query = ProbeQueryWire::latest(3, HashedId::new(9), &[HashedId::new(1)]);
        server.publish_query(query).unwrap();
        let forwarded = arb.try_query().unwrap().expect("query s2a");
        arb.publish_query_to_nodes(forwarded).unwrap();
        let at_node = node_q.try_query().unwrap().expect("query a2n");
        assert_eq!(at_node.request_id, 3);
        let rec = |vt: u64, value: u16| ProbeRecord {
            probe_id: HashedId::new(1),
            virtual_time_ns: vt,
            kind: ProbeKind::Digital,
            value,
        };
        node_q
            .publish_reply(ProbeQueryReplyWire::chunk(
                3,
                HashedId::new(9),
                &[rec(10, 1)],
                false,
                false,
            ))
            .unwrap();
        node_q
            .publish_reply(ProbeQueryReplyWire::chunk(
                3,
                HashedId::new(9),
                &[rec(11, 2)],
                true,
                false,
            ))
            .unwrap();
        let first = arb.try_node_reply().unwrap().expect("n2a first");
        arb.publish_reply_to_server(first).unwrap();
        let second = arb.try_node_reply().unwrap().expect("n2a last");
        arb.publish_reply_to_server(second).unwrap();
        let reply = server.try_reply().unwrap().expect("a2s first");
        assert!(!reply.is_last());
        assert_eq!(reply.count, 1);
        let last = server.try_reply().unwrap().expect("a2s last");
        assert!(last.is_last());
        assert_eq!(last.count, 1);
        let records: Vec<_> = last.records().collect();
        assert_eq!(records[0].value, 2);

        let node_csv = NodeProbeCsv::open(&node, &key, 1).unwrap();
        server
            .publish_csv(ProbeQueryWire::csv(
                4,
                HashedId::new(9),
                &[HashedId::new(1)],
                None,
                None,
            ))
            .unwrap();
        let csv = arb.try_csv().unwrap().expect("csv s2a");
        arb.publish_csv_to_nodes(csv).unwrap();
        let at_csv = node_csv.try_query().unwrap().expect("csv a2n");
        assert_eq!(at_csv.request_id, 4);
        node_csv
            .publish_reply(ProbeQueryReplyWire::chunk(
                4,
                HashedId::new(9),
                &[],
                true,
                false,
            ))
            .unwrap();
        let csv_up = arb.try_node_csv().unwrap().expect("csv n2a");
        arb.publish_csv_to_server(csv_up).unwrap();
        let csv_reply = server.try_csv_reply().unwrap().expect("csv a2s");
        assert!(csv_reply.is_last());
    }
}
