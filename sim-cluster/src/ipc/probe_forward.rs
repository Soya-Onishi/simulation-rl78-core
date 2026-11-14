//! Probe catalog/sample payloads formerly published on iceoryx `probe_catalog/a2s`
//! and `probe/a2s`. Transport to the server is now mpsc ([`super::server_arbiter`]).

use crate::probe::PROBE_NAME_CAP;

pub const MSG_CATALOG: u8 = 1;
pub const MSG_SAMPLE: u8 = 2;
pub const MSG_CATALOG_END: u8 = 3;

/// Arbiter → server probe catalog or live sample payload.
///
/// TODO(#126): layout is still iceoryx/`ZeroCopySend`-shaped (`#[repr(C)]`, fixed
/// `name` buffer + `name_len`, `_pad`, raw `msg`/`wire_kind` bytes) even though
/// transport is now mpsc. Prefer `String`, typed enums, drop padding/`msg` in
/// favor of `ArbiterToServer` variants, and split catalog vs sample payloads.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ProbeToServer {
    pub probe_id: crate::ipc::HashedId<u64>,
    pub board_hash: crate::ipc::HashedId<u64>,
    pub virtual_time_ns: u64,
    pub value: u16,
    /// TODO(#126): redundant with `ArbiterToServer::{ProbeCatalog, ProbeSample}`;
    /// catalog-end should not be a `msg` discriminant inside this struct.
    pub msg: u8,
    /// TODO(#126): use `ProbeWireKind` / `ProbeKind` instead of a raw `u8`.
    pub wire_kind: u8,
    /// TODO(#126): replace `name_len` + fixed `name` with `String`.
    pub name_len: u8,
    /// TODO(#126): iceoryx alignment padding; remove with `#[repr(C)]`.
    pub _pad: [u8; 3],
    pub name: [u8; PROBE_NAME_CAP],
}
