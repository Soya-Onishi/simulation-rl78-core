//! Cross-machine arbiter networking (listen sockets, time-sync multicast, control TCP, data UDP).

mod control_tcp;
mod data_udp;
mod listen;
mod time_mcast;

pub use control_tcp::{
    ControlMesh, ControlMeshError, ControlMessage, ControlPacket, MeshNotice, MeshState,
};
pub use data_udp::{
    DEFAULT_DATA_UDP_WINDOW, DataLink, DataLinkError, DataLinkStats, EdgeMessage, EdgeValue,
    PENDING_LIMIT, Packet, RETRANSMIT_INTERVAL, RecvOutcome, SlidingWindowRecv, SlidingWindowSend,
};
pub use listen::{PeerListenError, PeerListeners};
pub use time_mcast::{
    PEER_SILENCE, RemoteTimes, TimeMcast, TimeMcastError, TimeReportPacket, TimeSyncState,
};
