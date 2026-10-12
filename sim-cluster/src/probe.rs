//! Runtime probe publishers.
//!
//! Wiring taps ([`sim_kernel::ProbeTap`]) are created with the board. [`ProbePublishers::open`]
//! turns the returned [`sim_kernel::ProbePoint`] list into iceoryx publishers.

use iceoryx2::prelude::Node;

use sim_kernel::{DigitalLevel, ProbePoint, ProbeTap, UartFrame};

use crate::ipc::{
    IpcError, ProbeDigitalPub, ProbeDigitalSample, ProbeUartPub, ProbeUartSample, board_id_hash,
    open_digital_publisher, open_uart_publisher, probe_id_hash, probe_name_hash, send_digital,
    send_uart,
};

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
}

impl ProbeWireKind {
    #[must_use]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Digital),
            2 => Some(Self::Uart),
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
            }
        }
        Ok(Self { slots, ads })
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

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{DigitalLevel, ProbePolicy, ProbeTap};

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
