//! Probe catalog/sample payloads formerly published on iceoryx `probe_catalog/a2s`
//! and `probe/a2s`. Transport to the server is now mpsc ([`super::server_arbiter`]).

use crate::probe::PROBE_NAME_CAP;

pub const MSG_CATALOG: u8 = 1;
pub const MSG_SAMPLE: u8 = 2;
pub const MSG_CATALOG_END: u8 = 3;

/// Arbiter → server probe catalog or live sample payload.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ProbeToServer {
    pub probe_id: crate::ipc::HashedId<u64>,
    pub board_hash: crate::ipc::HashedId<u64>,
    pub virtual_time_ns: u64,
    pub value: u16,
    pub msg: u8,
    pub wire_kind: u8,
    pub name_len: u8,
    pub _pad: [u8; 3],
    pub name: [u8; PROBE_NAME_CAP],
}
