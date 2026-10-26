//! Arbiter-side probe catalog and selective subscribe / drain.

use std::collections::HashMap;

use iceoryx2::prelude::Node;

use crate::ipc::{
    IpcError, ProbeAnalogSample, ProbeAnalogSub, ProbeDigitalSample, ProbeDigitalSub,
    ProbeUartSample, ProbeUartSub, open_analog_subscriber, open_digital_subscriber,
    open_uart_subscriber, probe_id_hash,
};
use crate::probe::{PROBE_NAME_CAP, ProbeAdvertisement, ProbeWireKind};

/// One advertised probe known to the arbiter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeCatalogEntry {
    pub board_id: String,
    pub board_hash: u64,
    pub name: String,
    pub name_hash: u64,
    pub probe_id: u64,
    pub kind: ProbeWireKind,
}

/// Drained probe sample (typed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeSample {
    Digital(ProbeDigitalSample),
    Uart(ProbeUartSample),
    Analog(ProbeAnalogSample),
}

/// Failures from arbiter probe subscribe helpers.
#[derive(Debug, thiserror::Error)]
pub enum ArbiterProbeError {
    #[error("unknown probe id {0:#x}")]
    UnknownProbe(u64),
    #[error("unknown probe {board_id}/{name}")]
    UnknownName { board_id: String, name: String },
    #[error("probe {0:#x} already subscribed")]
    AlreadySubscribed(u64),
    #[error(transparent)]
    Ipc(#[from] IpcError),
}

enum LiveSub {
    Digital(ProbeDigitalSub),
    Uart(ProbeUartSub),
    Analog(ProbeAnalogSub),
}

/// Catalog + optional live subscribers for probe samples.
pub struct ArbiterProbeHub {
    cluster_key: String,
    catalog: HashMap<u64, ProbeCatalogEntry>,
    by_name: HashMap<(String, String), u64>,
    subs: HashMap<u64, LiveSub>,
}

impl ArbiterProbeHub {
    #[must_use]
    pub fn new(cluster_key: impl Into<String>) -> Self {
        Self {
            cluster_key: cluster_key.into(),
            catalog: HashMap::new(),
            by_name: HashMap::new(),
            subs: HashMap::new(),
        }
    }

    /// Record a node [`ProbeAdvertise`](crate::control::ControlToArbiter::ProbeAdvertise).
    pub fn record_advertise(
        &mut self,
        board_id: impl Into<String>,
        board_hash: u64,
        adv: ProbeAdvertisement,
    ) {
        let board_id = board_id.into();
        let entry = ProbeCatalogEntry {
            board_id: board_id.clone(),
            board_hash,
            name: adv.name.clone(),
            name_hash: adv.name_hash,
            probe_id: adv.probe_id,
            kind: adv.kind,
        };
        self.by_name.insert((board_id, adv.name), adv.probe_id);
        self.catalog.insert(adv.probe_id, entry);
    }

    /// Record from a raw control-plane advertise payload.
    pub fn record_wire_advertise(
        &mut self,
        board_id: impl Into<String>,
        board_hash: u64,
        name_hash: u64,
        probe_id: u64,
        kind: ProbeWireKind,
        name_bytes: &[u8],
    ) {
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        self.record_advertise(
            board_id,
            board_hash,
            ProbeAdvertisement {
                name,
                name_hash,
                probe_id,
                kind,
            },
        );
    }

    #[must_use]
    pub fn catalog(&self) -> &HashMap<u64, ProbeCatalogEntry> {
        &self.catalog
    }

    /// Open an iceoryx subscriber for `probe_id` (no-op path if already open → error).
    pub fn subscribe_probe(
        &mut self,
        node: &Node<iceoryx2::prelude::ipc::Service>,
        probe_id: u64,
    ) -> Result<(), ArbiterProbeError> {
        if self.subs.contains_key(&probe_id) {
            return Err(ArbiterProbeError::AlreadySubscribed(probe_id));
        }
        let entry = self
            .catalog
            .get(&probe_id)
            .ok_or(ArbiterProbeError::UnknownProbe(probe_id))?
            .clone();
        let sub = match entry.kind {
            ProbeWireKind::Digital => LiveSub::Digital(open_digital_subscriber(
                node,
                &self.cluster_key,
                entry.board_hash,
                entry.name_hash,
            )?),
            ProbeWireKind::Uart => LiveSub::Uart(open_uart_subscriber(
                node,
                &self.cluster_key,
                entry.board_hash,
                entry.name_hash,
            )?),
            ProbeWireKind::Analog => LiveSub::Analog(open_analog_subscriber(
                node,
                &self.cluster_key,
                entry.board_hash,
                entry.name_hash,
            )?),
        };
        self.subs.insert(probe_id, sub);
        Ok(())
    }

    /// Resolve `(board_id, name)` then [`subscribe_probe`].
    pub fn subscribe_by_name(
        &mut self,
        node: &Node<iceoryx2::prelude::ipc::Service>,
        board_id: &str,
        name: &str,
    ) -> Result<u64, ArbiterProbeError> {
        let probe_id = self
            .by_name
            .get(&(board_id.to_string(), name.to_string()))
            .copied()
            .or_else(|| {
                // Allow subscribe before advertise using deterministic hash, if kind is known
                // only via catalog — without catalog we cannot open the right typed sub.
                let id = probe_id_hash(board_id, name);
                self.catalog.contains_key(&id).then_some(id)
            })
            .ok_or_else(|| ArbiterProbeError::UnknownName {
                board_id: board_id.to_string(),
                name: name.to_string(),
            })?;
        self.subscribe_probe(node, probe_id)?;
        Ok(probe_id)
    }

    /// Subscribe every catalog entry. Call after the ready barrier.
    pub fn subscribe_catalog(
        &mut self,
        node: &Node<iceoryx2::prelude::ipc::Service>,
    ) -> Result<(), ArbiterProbeError> {
        let ids: Vec<u64> = self.catalog.keys().copied().collect();
        for id in ids {
            self.subscribe_probe(node, id)?;
        }
        Ok(())
    }

    pub fn unsubscribe_probe(&mut self, probe_id: u64) -> bool {
        self.subs.remove(&probe_id).is_some()
    }

    /// Non-blocking drain of all active subscriptions.
    pub fn drain(&self, limit_per_probe: usize) -> Result<Vec<ProbeSample>, IpcError> {
        let mut out = Vec::new();
        for sub in self.subs.values() {
            match sub {
                LiveSub::Digital(s) => {
                    for _ in 0..limit_per_probe {
                        match s
                            .receive()
                            .map_err(|e| IpcError::Message(format!("probe digital recv: {e:?}")))?
                        {
                            Some(sample) => out.push(ProbeSample::Digital(*sample)),
                            None => break,
                        }
                    }
                }
                LiveSub::Uart(s) => {
                    for _ in 0..limit_per_probe {
                        match s
                            .receive()
                            .map_err(|e| IpcError::Message(format!("probe uart recv: {e:?}")))?
                        {
                            Some(sample) => out.push(ProbeSample::Uart(*sample)),
                            None => break,
                        }
                    }
                }
                LiveSub::Analog(s) => {
                    for _ in 0..limit_per_probe {
                        match s
                            .receive()
                            .map_err(|e| IpcError::Message(format!("probe analog recv: {e:?}")))?
                        {
                            Some(sample) => out.push(ProbeSample::Analog(*sample)),
                            None => break,
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Pack a probe name into the fixed control-plane buffer.
#[must_use]
pub fn pack_probe_name(name: &str) -> ([u8; PROBE_NAME_CAP], u8) {
    let mut buf = [0u8; PROBE_NAME_CAP];
    let n = name.len().min(PROBE_NAME_CAP);
    buf[..n].copy_from_slice(&name.as_bytes()[..n]);
    (buf, n as u8)
}

/// Unpack a probe name from the control-plane buffer.
#[must_use]
pub fn unpack_probe_name(buf: &[u8; PROBE_NAME_CAP], len: u8) -> String {
    let n = (len as usize).min(PROBE_NAME_CAP);
    String::from_utf8_lossy(&buf[..n]).into_owned()
}
