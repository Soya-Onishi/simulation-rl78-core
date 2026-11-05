//! Bind control-TCP and data-UDP sockets for arbiter peer exchange (phase-1 advertise).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, UdpSocket};

use crate::orchestrate::{ArbiterEndpoints, BackendKey};

/// Failed to bind a peer listen socket.
#[derive(Debug, thiserror::Error)]
pub enum PeerListenError {
    #[error("invalid advertise_ip `{0}`: expected IPv4")]
    BadAdvertiseIp(String),
    #[error("bind control TCP: {0}")]
    ControlTcp(std::io::Error),
    #[error("bind data UDP: {0}")]
    DataUdp(std::io::Error),
}

/// Bound peer sockets held for the life of a simulation arbiter session.
pub struct PeerListeners {
    pub control_tcp: TcpListener,
    pub data_udp: UdpSocket,
    endpoints: ArbiterEndpoints,
}

impl PeerListeners {
    /// Bind ephemeral ports on `0.0.0.0` and advertise them via `advertise_ip`.
    pub fn bind(backend: BackendKey, advertise_ip: &str) -> Result<Self, PeerListenError> {
        let ip: Ipv4Addr = advertise_ip
            .parse()
            .map_err(|_| PeerListenError::BadAdvertiseIp(advertise_ip.to_string()))?;
        if ip.is_unspecified() || ip.is_multicast() {
            return Err(PeerListenError::BadAdvertiseIp(advertise_ip.to_string()));
        }

        let control_tcp = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
            .map_err(PeerListenError::ControlTcp)?;
        control_tcp
            .set_nonblocking(true)
            .map_err(PeerListenError::ControlTcp)?;
        let control_port = match control_tcp
            .local_addr()
            .map_err(PeerListenError::ControlTcp)?
        {
            SocketAddr::V4(a) => a.port(),
            SocketAddr::V6(_) => {
                return Err(PeerListenError::ControlTcp(std::io::Error::other(
                    "expected IPv4 TCP listener",
                )));
            }
        };

        let data_udp = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
            .map_err(PeerListenError::DataUdp)?;
        data_udp
            .set_nonblocking(true)
            .map_err(PeerListenError::DataUdp)?;
        let data_port = match data_udp.local_addr().map_err(PeerListenError::DataUdp)? {
            SocketAddr::V4(a) => a.port(),
            SocketAddr::V6(_) => {
                return Err(PeerListenError::DataUdp(std::io::Error::other(
                    "expected IPv4 UDP socket",
                )));
            }
        };

        Ok(Self {
            control_tcp,
            data_udp,
            endpoints: ArbiterEndpoints {
                backend,
                advertise_ip: advertise_ip.to_string(),
                control_tcp_port: control_port,
                data_udp_port: data_port,
            },
        })
    }

    #[must_use]
    pub fn endpoints(&self) -> &ArbiterEndpoints {
        &self.endpoints
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_returns_nonzero_ports_and_advertise_ip() {
        let listeners = PeerListeners::bind("127.0.0.1:8091".into(), "127.0.0.1").unwrap();
        let ep = listeners.endpoints();
        assert_eq!(ep.backend, "127.0.0.1:8091");
        assert_eq!(ep.advertise_ip, "127.0.0.1");
        assert_ne!(ep.control_tcp_port, 0);
        assert_ne!(ep.data_udp_port, 0);

        let _tcp = std::net::TcpStream::connect(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            ep.control_tcp_port,
        ))
        .unwrap();
        let probe = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        probe
            .send_to(
                b"x",
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, ep.data_udp_port),
            )
            .unwrap();
    }

    #[test]
    fn bind_peer_listeners_none_skips() {
        use crate::arbiter::{ArbiterOptions, bind_peer_listeners};
        use std::path::PathBuf;
        let opts = ArbiterOptions {
            topology_path: PathBuf::from("t.json"),
            node_bin: PathBuf::from("/tmp/rl78"),
            plant_bin: PathBuf::from("/tmp/plant"),
            cluster_key: "k".into(),
            iox_root: PathBuf::from("/tmp/iox"),
            local_board_ids: None,
            peer_advertise: None,
            endpoints_out: None,
            phase2_in: None,
            time_sync_period_ms: 100,
            assignment_in: None,
            data_udp_window: 32,
        };
        assert!(bind_peer_listeners(&opts).unwrap().is_none());
    }

    #[test]
    fn bind_peer_listeners_writes_endpoints_file() {
        use crate::arbiter::{ArbiterOptions, PeerAdvertise, bind_peer_listeners};
        use std::path::PathBuf;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("endpoints.json");
        let opts = ArbiterOptions {
            topology_path: PathBuf::from("t.json"),
            node_bin: PathBuf::from("/tmp/rl78"),
            plant_bin: PathBuf::from("/tmp/plant"),
            cluster_key: "k".into(),
            iox_root: PathBuf::from("/tmp/iox"),
            local_board_ids: None,
            peer_advertise: Some(PeerAdvertise {
                advertise_ip: "127.0.0.1".into(),
                backend_key: "127.0.0.1:8091".into(),
            }),
            endpoints_out: Some(out.clone()),
            phase2_in: None,
            time_sync_period_ms: 100,
            assignment_in: None,
            data_udp_window: 32,
        };
        let listeners = bind_peer_listeners(&opts).unwrap().unwrap();
        let written: ArbiterEndpoints =
            serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(&written, listeners.endpoints());
        assert_eq!(written.backend, "127.0.0.1:8091");
        assert!(!out.with_extension("tmp").exists());
    }

    #[test]
    fn reject_unspecified_and_multicast_advertise_ip() {
        assert!(PeerListeners::bind("x".into(), "0.0.0.0").is_err());
        assert!(PeerListeners::bind("x".into(), "239.255.0.1").is_err());
        assert!(PeerListeners::bind("x".into(), "not-an-ip").is_err());
    }
}
