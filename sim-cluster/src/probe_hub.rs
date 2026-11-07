//! Arbiter-side probe catalog and selective subscribe / drain.

use std::collections::HashMap;

use iceoryx2::prelude::Node;

use crate::ipc::{
    IpcError, ProbeAnalogSample, ProbeAnalogSub, ProbeDigitalSample, ProbeDigitalSub,
    ProbeUartSample, ProbeUartSub, open_analog_subscriber, open_digital_subscriber,
    open_uart_subscriber, probe_id_hash,
};
use crate::probe::{ProbeAdvertisement, ProbeWireKind};

/// One advertised probe known to the arbiter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeCatalogEntry {
    pub board_id: String,
    pub board_hash: crate::ipc::HashedId<u64>,
    pub name: String,
    pub name_hash: crate::ipc::HashedId<u64>,
    pub probe_id: crate::ipc::HashedId<u64>,
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
    #[error("unknown probe id {0}")]
    UnknownProbe(crate::ipc::HashedId<u64>),
    #[error("unknown probe {board_id}/{name}")]
    UnknownName { board_id: String, name: String },
    #[error("probe {0} already subscribed")]
    AlreadySubscribed(crate::ipc::HashedId<u64>),
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
    catalog: HashMap<crate::ipc::HashedId<u64>, ProbeCatalogEntry>,
    by_name: HashMap<(crate::ipc::HashedId<u64>, String), crate::ipc::HashedId<u64>>,
    subs: HashMap<crate::ipc::HashedId<u64>, LiveSub>,
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

    /// Record one probe into the catalog (from `--list-catalog` or tests).
    pub fn record_advertise(
        &mut self,
        board_id: impl Into<String>,
        board_hash: crate::ipc::HashedId<u64>,
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
        self.by_name.insert((board_hash, adv.name), adv.probe_id);
        self.catalog.insert(adv.probe_id, entry);
    }

    #[must_use]
    pub fn catalog(&self) -> &HashMap<crate::ipc::HashedId<u64>, ProbeCatalogEntry> {
        &self.catalog
    }

    /// Open an iceoryx subscriber for `probe_id` (no-op path if already open → error).
    pub fn subscribe_probe(
        &mut self,
        node: &Node<iceoryx2::prelude::ipc::Service>,
        probe_id: crate::ipc::HashedId<u64>,
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

    /// Resolve `(board_hash, name)` then [`subscribe_probe`].
    pub fn subscribe_by_name(
        &mut self,
        node: &Node<iceoryx2::prelude::ipc::Service>,
        board_hash: crate::ipc::HashedId<u64>,
        name: &str,
    ) -> Result<crate::ipc::HashedId<u64>, ArbiterProbeError> {
        let probe_id = self
            .by_name
            .get(&(board_hash, name.to_string()))
            .copied()
            .or_else(|| {
                // Allow subscribe before advertise using deterministic hash, if kind is known
                // only via catalog — without catalog we cannot open the right typed sub.
                let id = probe_id_hash(board_hash, name);
                self.catalog.contains_key(&id).then_some(id)
            })
            .ok_or_else(|| ArbiterProbeError::UnknownName {
                board_id: self
                    .catalog
                    .values()
                    .find(|e| e.board_hash == board_hash)
                    .map(|e| e.board_id.clone())
                    .unwrap_or_else(|| board_hash.to_string()),
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
        let ids: Vec<crate::ipc::HashedId<u64>> = self.catalog.keys().copied().collect();
        for id in ids {
            self.subscribe_probe(node, id)?;
        }
        Ok(())
    }

    pub fn unsubscribe_probe(&mut self, probe_id: crate::ipc::HashedId<u64>) -> bool {
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
