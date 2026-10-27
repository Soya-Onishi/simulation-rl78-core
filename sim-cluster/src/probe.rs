//! Runtime probe publishers.
//!
//! Wiring taps ([`sim_kernel::ProbeTap`]) are created with the board. [`ProbePublishers::open`]
//! turns the returned [`sim_kernel::ProbePoint`] list into iceoryx publishers.

use iceoryx2::prelude::Node;

use sim_kernel::{AnalogVoltage, DigitalLevel, ProbePoint, ProbeTap, UartFrame};

use crate::ipc::{
    CSV_CHUNKS_PER_TICK, IpcError, NodeProbeCsv, NodeProbeQuery, ProbeAnalogPub, ProbeAnalogSample,
    ProbeDigitalPub, ProbeDigitalSample, ProbeQueryReplyWire, ProbeUartPub, ProbeUartSample,
    QUERY_CHUNK, QUERY_KIND_LATEST, board_id_hash, open_analog_publisher, open_digital_publisher,
    open_uart_publisher, probe_id_hash, probe_name_hash, send_analog, send_digital, send_uart,
};
use crate::probe_log::{ProbeKind, ProbeLog, ProbePointName, ProbeRange, ProbeRecord, load_export};

/// Max probe points registered on one board (advertise is not fragmented).
pub const MAX_PROBES_PER_BOARD: usize = 32;

/// Max UTF-8 bytes stored for a probe name on the control plane.
pub const PROBE_NAME_CAP: usize = 64;

/// Wire kind carried on [`crate::control::ControlToArbiter::ProbeAdvertise`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeWireKind {
    Digital = 1,
    Uart = 2,
    Analog = 3,
}

impl ProbeWireKind {
    #[must_use]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Digital),
            2 => Some(Self::Uart),
            3 => Some(Self::Analog),
            _ => None,
        }
    }
}

/// Metadata recorded for [`ControlToArbiter::ProbeAdvertise`](crate::control::ControlToArbiter).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeAdvertisement {
    pub name: String,
    pub name_hash: u64,
    pub probe_id: u64,
    pub kind: ProbeWireKind,
}

/// Errors while opening runtime probe publishers.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("probe name exceeds {PROBE_NAME_CAP} UTF-8 bytes")]
    NameTooLong,
    #[error("probe name already registered on this board: {0}")]
    DuplicateName(String),
    #[error("board already has {MAX_PROBES_PER_BOARD} probes")]
    TooManyProbes,
    #[error(transparent)]
    Ipc(#[from] IpcError),
}

enum PublisherSlot {
    Digital {
        tap: std::sync::Arc<ProbeTap<DigitalLevel>>,
        probe_id: u64,
        publisher: ProbeDigitalPub,
    },
    Uart {
        tap: std::sync::Arc<ProbeTap<UartFrame>>,
        probe_id: u64,
        publisher: ProbeUartPub,
    },
    Analog {
        tap: std::sync::Arc<ProbeTap<AnalogVoltage>>,
        probe_id: u64,
        publisher: ProbeAnalogPub,
    },
}

fn analog_millivolts(uv: i64) -> u16 {
    uv.saturating_div(1000).clamp(0, i64::from(u16::MAX)) as u16
}

/// Iceoryx publishers for the probe points a board builder returned.
pub struct ProbePublishers {
    slots: Vec<PublisherSlot>,
    ads: Vec<ProbeAdvertisement>,
}

impl ProbePublishers {
    /// Open one publisher per wiring tap. `points` is the board builder's probe list.
    pub fn open(
        node: &Node<iceoryx2::prelude::ipc::Service>,
        cluster_key: &str,
        board_id: &str,
        points: Vec<ProbePoint>,
    ) -> Result<Self, ProbeError> {
        let ads = plan(board_id, &points)?;
        let board_hash = board_id_hash(board_id);
        let mut slots = Vec::with_capacity(points.len());
        for (point, adv) in points.into_iter().zip(ads.iter()) {
            match point {
                ProbePoint::Digital(tap) => {
                    let publisher =
                        open_digital_publisher(node, cluster_key, board_hash, adv.name_hash)?;
                    slots.push(PublisherSlot::Digital {
                        tap,
                        probe_id: adv.probe_id,
                        publisher,
                    });
                }
                ProbePoint::Uart(tap) => {
                    let publisher =
                        open_uart_publisher(node, cluster_key, board_hash, adv.name_hash)?;
                    slots.push(PublisherSlot::Uart {
                        tap,
                        probe_id: adv.probe_id,
                        publisher,
                    });
                }
                ProbePoint::Analog(tap) => {
                    let publisher =
                        open_analog_publisher(node, cluster_key, board_hash, adv.name_hash)?;
                    slots.push(PublisherSlot::Analog {
                        tap,
                        probe_id: adv.probe_id,
                        publisher,
                    });
                }
            }
        }
        Ok(Self { slots, ads })
    }

    /// Store samples accepted by the wiring taps since the last pump.
    pub fn ingest(&self, log: &mut ProbeLog) {
        for slot in &self.slots {
            match slot {
                PublisherSlot::Digital { tap, probe_id, .. } => {
                    if let Some(sample) = tap.take_pending() {
                        log.push(ProbeRecord {
                            probe_id: *probe_id,
                            virtual_time_ns: sample.virtual_time_ns,
                            kind: ProbeKind::Digital,
                            value: u16::from(sample.value as u8),
                        });
                    }
                }
                PublisherSlot::Uart { tap, probe_id, .. } => {
                    if let Some(sample) = tap.take_pending() {
                        log.push(ProbeRecord {
                            probe_id: *probe_id,
                            virtual_time_ns: sample.virtual_time_ns,
                            kind: ProbeKind::Uart,
                            value: sample.value.data,
                        });
                    }
                }
                PublisherSlot::Analog { tap, probe_id, .. } => {
                    if let Some(sample) = tap.take_pending() {
                        log.push(ProbeRecord {
                            probe_id: *probe_id,
                            virtual_time_ns: sample.virtual_time_ns,
                            kind: ProbeKind::Analog,
                            value: analog_millivolts(sample.value.0),
                        });
                    }
                }
            }
        }
    }

    pub fn publish_record(&self, record: &ProbeRecord) {
        for slot in &self.slots {
            match slot {
                PublisherSlot::Digital {
                    probe_id,
                    publisher,
                    ..
                } if *probe_id == record.probe_id => {
                    let level = match record.value {
                        1 => DigitalLevel::High,
                        2 => DigitalLevel::Low,
                        _ => DigitalLevel::HiZ,
                    };
                    send_digital(
                        publisher,
                        ProbeDigitalSample::new(record.probe_id, record.virtual_time_ns, level),
                    );
                    return;
                }
                PublisherSlot::Uart {
                    probe_id,
                    publisher,
                    ..
                } if *probe_id == record.probe_id => {
                    send_uart(
                        publisher,
                        ProbeUartSample {
                            probe_id: record.probe_id,
                            virtual_time_ns: record.virtual_time_ns,
                            value: UartFrame {
                                data: record.value,
                                ..UartFrame::default()
                            },
                        },
                    );
                    return;
                }
                PublisherSlot::Analog {
                    probe_id,
                    publisher,
                    ..
                } if *probe_id == record.probe_id => {
                    send_analog(
                        publisher,
                        ProbeAnalogSample {
                            probe_id: record.probe_id,
                            virtual_time_ns: record.virtual_time_ns,
                            value: AnalogVoltage(i64::from(record.value).saturating_mul(1000)),
                        },
                    );
                    return;
                }
                _ => {}
            }
        }
    }

    /// Publish samples accepted by the wiring taps since the last pump.
    pub fn pump(&self) {
        for slot in &self.slots {
            match slot {
                PublisherSlot::Digital {
                    tap,
                    probe_id,
                    publisher,
                } => {
                    if let Some(sample) = tap.take_pending() {
                        send_digital(
                            publisher,
                            ProbeDigitalSample::new(
                                *probe_id,
                                sample.virtual_time_ns,
                                sample.value,
                            ),
                        );
                    }
                }
                PublisherSlot::Uart {
                    tap,
                    probe_id,
                    publisher,
                } => {
                    if let Some(sample) = tap.take_pending() {
                        send_uart(
                            publisher,
                            ProbeUartSample {
                                probe_id: *probe_id,
                                virtual_time_ns: sample.virtual_time_ns,
                                value: sample.value,
                            },
                        );
                    }
                }
                PublisherSlot::Analog {
                    tap,
                    probe_id,
                    publisher,
                } => {
                    if let Some(sample) = tap.take_pending() {
                        send_analog(
                            publisher,
                            ProbeAnalogSample {
                                probe_id: *probe_id,
                                virtual_time_ns: sample.virtual_time_ns,
                                value: sample.value,
                            },
                        );
                    }
                }
            }
        }
    }

    #[must_use]
    pub fn advertisements(&self) -> &[ProbeAdvertisement] {
        &self.ads
    }
}

fn plan(board_id: &str, points: &[ProbePoint]) -> Result<Vec<ProbeAdvertisement>, ProbeError> {
    if points.len() > MAX_PROBES_PER_BOARD {
        return Err(ProbeError::TooManyProbes);
    }
    let mut ads = Vec::with_capacity(points.len());
    for point in points {
        let name = point.name();
        if name.len() > PROBE_NAME_CAP {
            return Err(ProbeError::NameTooLong);
        }
        if ads.iter().any(|adv: &ProbeAdvertisement| adv.name == name) {
            return Err(ProbeError::DuplicateName(name.to_string()));
        }
        let kind = match point {
            ProbePoint::Digital(_) => ProbeWireKind::Digital,
            ProbePoint::Uart(_) => ProbeWireKind::Uart,
            ProbePoint::Analog(_) => ProbeWireKind::Analog,
        };
        ads.push(ProbeAdvertisement {
            name: name.to_string(),
            name_hash: probe_name_hash(name),
            probe_id: probe_id_hash(board_id, name),
            kind,
        });
    }
    Ok(ads)
}

struct CsvSend {
    request_id: u64,
    records: Vec<ProbeRecord>,
    offset: usize,
    truncated: bool,
}

/// Node-owned probe store, watch set, coarse emit, and query replies.
pub struct NodeProbeRuntime {
    publishers: ProbePublishers,
    log: ProbeLog,
    watch: std::collections::HashSet<u64>,
    board_hash: u64,
    query: NodeProbeQuery,
    csv: NodeProbeCsv,
    csv_out: Option<CsvSend>,
}

impl NodeProbeRuntime {
    pub fn open(
        node: &iceoryx2::prelude::Node<iceoryx2::prelude::ipc::Service>,
        cluster_key: &str,
        board_id: &str,
        board_count: usize,
        points: Vec<ProbePoint>,
    ) -> Result<Self, ProbeError> {
        let publishers = ProbePublishers::open(node, cluster_key, board_id, points)?;
        let mut log = ProbeLog::open(cluster_key, board_id);
        for adv in publishers.advertisements() {
            log.note_catalog(ProbePointName {
                probe_id: adv.probe_id,
                board_id: board_id.to_string(),
                name: adv.name.clone(),
                kind: match adv.kind {
                    ProbeWireKind::Digital => ProbeKind::Digital,
                    ProbeWireKind::Uart => ProbeKind::Uart,
                    ProbeWireKind::Analog => ProbeKind::Analog,
                },
            });
        }
        Ok(Self {
            publishers,
            log,
            watch: std::collections::HashSet::new(),
            board_hash: board_id_hash(board_id),
            query: NodeProbeQuery::open(node, cluster_key, board_count)?,
            csv: NodeProbeCsv::open(node, cluster_key, board_count)?,
            csv_out: None,
        })
    }

    #[must_use]
    pub fn advertisements(&self) -> &[ProbeAdvertisement] {
        self.publishers.advertisements()
    }

    pub fn set_watch(&mut self, board_hash: u64, probe_ids: &[u64]) {
        if board_hash != self.board_hash {
            return;
        }
        self.watch = probe_ids.iter().copied().collect();
    }

    pub fn set_running(&mut self, running: bool) {
        let flushed = self.log.set_running(running);
        self.publish_watched(&flushed);
    }

    pub fn ingest_and_emit(&mut self) {
        self.publishers.ingest(&mut self.log);
        let records = self.log.poll_coarse(std::time::Instant::now());
        self.publish_watched(&records);
    }

    pub fn service_queries(&mut self) -> Result<(), IpcError> {
        for _ in 0..8 {
            match self.query.try_query()? {
                Some(query) => self.answer_latest(query)?,
                None => break,
            }
        }
        Ok(())
    }

    pub fn service_csv(&mut self) -> Result<(), IpcError> {
        loop {
            match self.csv.try_query()? {
                Some(query) if self.csv_out.is_none() => self.begin_csv(query)?,
                Some(_) => {}
                None => break,
            }
        }
        self.send_csv_tick()
    }

    fn publish_watched(&self, records: &[ProbeRecord]) {
        for record in records {
            if self.watch.contains(&record.probe_id) {
                self.publishers.publish_record(record);
            }
        }
    }

    fn answer_latest(&mut self, query: crate::ipc::ProbeQueryWire) -> Result<(), IpcError> {
        if query.board_hash != 0 && query.board_hash != self.board_hash {
            return Ok(());
        }
        if query.kind != QUERY_KIND_LATEST {
            return Ok(());
        }
        let records = self.log.latest_of(query.probe_ids());
        self.send_latest_reply(query.request_id, &records)
    }

    fn send_latest_reply(&self, request_id: u64, records: &[ProbeRecord]) -> Result<(), IpcError> {
        if records.is_empty() {
            return self.query.publish_reply(ProbeQueryReplyWire::chunk(
                request_id,
                self.board_hash,
                &[],
                true,
                false,
            ));
        }
        let mut offset = 0;
        while offset < records.len() {
            let end = (offset + QUERY_CHUNK).min(records.len());
            let last = end == records.len();
            self.query.publish_reply(ProbeQueryReplyWire::chunk(
                request_id,
                self.board_hash,
                &records[offset..end],
                last,
                false,
            ))?;
            offset = end;
        }
        Ok(())
    }

    fn begin_csv(&mut self, query: crate::ipc::ProbeQueryWire) -> Result<(), IpcError> {
        if query.board_hash != 0 && query.board_hash != self.board_hash {
            return Ok(());
        }
        let range = ProbeRange {
            from_ns: (query.has_from != 0).then_some(query.from_ns),
            to_ns: (query.has_to != 0).then_some(query.to_ns),
        };
        let wanted: std::collections::HashSet<u64> = query.probe_ids().iter().copied().collect();
        let (records, truncated) = match self.log.export() {
            Ok(export) => {
                let truncated = export.truncated;
                match load_export(&export) {
                    Ok(mut records) => {
                        records.retain(|rec| {
                            range.contains(rec.virtual_time_ns)
                                && (wanted.is_empty() || wanted.contains(&rec.probe_id))
                        });
                        (records, truncated)
                    }
                    Err(_) => (Vec::new(), true),
                }
            }
            Err(_) => (Vec::new(), true),
        };
        self.csv_out = Some(CsvSend {
            request_id: query.request_id,
            records,
            offset: 0,
            truncated,
        });
        Ok(())
    }

    fn send_csv_tick(&mut self) -> Result<(), IpcError> {
        for _ in 0..CSV_CHUNKS_PER_TICK {
            let Some(out) = self.csv_out.as_mut() else {
                break;
            };
            let end = (out.offset + QUERY_CHUNK).min(out.records.len());
            let last = end == out.records.len();
            self.csv.publish_reply(ProbeQueryReplyWire::chunk(
                out.request_id,
                self.board_hash,
                &out.records[out.offset..end],
                last,
                out.truncated,
            ))?;
            out.offset = end;
            if last {
                self.csv_out = None;
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{ProbePolicy, ProbeTap};

    fn digital(name: &str) -> ProbePoint {
        ProbePoint::Digital(ProbeTap::new(name, ProbePolicy::OnChange))
    }

    #[test]
    fn plan_rejects_too_many_and_duplicates() {
        let points: Vec<_> = (0..MAX_PROBES_PER_BOARD + 1)
            .map(|i| digital(&format!("p{i}")))
            .collect();
        assert!(matches!(
            plan("board", &points),
            Err(ProbeError::TooManyProbes)
        ));
        let dup = vec![digital("vcc"), digital("vcc")];
        assert!(matches!(
            plan("board", &dup),
            Err(ProbeError::DuplicateName(_))
        ));
    }
}
