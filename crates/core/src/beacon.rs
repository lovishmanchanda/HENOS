//! Beacons: HENOS's own way for devices on one network to find each other.
//!
//! A beacon is one small UDP packet saying "this device is here, on this
//! port". A device sends it to the whole local network; every HENOS device
//! that hears it answers the sender directly. So two devices learn about
//! each other within one round trip, and it still works when the broadcast
//! only gets through in one direction, which ordinary network discovery
//! often suffers from (Windows in particular).
//!
//! Like any network announcement, a beacon proves nothing. It is only a
//! hint about where to connect; the encrypted connection that follows is
//! what proves who the device is.

use std::{
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
};

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

/// The UDP port beacons are sent to and listened for on.
pub const BEACON_PORT: u16 = 47474;

const MAGIC: [u8; 4] = *b"HNS1";
/// Larger packets are not beacons.
pub(crate) const MAX_BEACON_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Beacon {
    magic: [u8; 4],
    /// The sender's device id.
    pub id: [u8; 32],
    /// The port the sender accepts encrypted connections on.
    pub port: u16,
    /// Name, platform and pairing flag, as in the other announcements.
    pub announcement: String,
    /// This beacon answers one of ours, so it must not be answered again.
    pub is_reply: bool,
}

impl Beacon {
    pub(crate) fn new(id: [u8; 32], port: u16, announcement: String, is_reply: bool) -> Self {
        Self {
            magic: MAGIC,
            id,
            port,
            announcement,
            is_reply,
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).unwrap_or_default()
    }

    /// Reads a packet. Anything that is not a well-formed beacon is `None`.
    pub(crate) fn decode(packet: &[u8]) -> Option<Self> {
        if packet.len() > MAX_BEACON_BYTES {
            return None;
        }
        let beacon: Self = postcard::from_bytes(packet).ok()?;
        (beacon.magic == MAGIC && beacon.port != 0).then_some(beacon)
    }
}

/// Opens the beacon socket on `port` (0 picks a free one).
///
/// Several HENOS programs on one computer can share the port, and the socket
/// is allowed to send to broadcast addresses.
pub(crate) fn bind(port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
    socket.set_reuse_port(true)?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).into())?;
    UdpSocket::from_std(socket.into())
}

/// Where to send a beacon so the whole local network hears it: the general
/// broadcast address, plus the broadcast address of each network this device
/// is on (some systems only deliver one or the other).
pub(crate) fn broadcast_targets(
    local: impl IntoIterator<Item = Ipv4Addr>,
    port: u16,
) -> Vec<SocketAddr> {
    let mut targets = vec![SocketAddr::from((Ipv4Addr::BROADCAST, port))];
    for ip in local {
        if !ip.is_private() {
            continue;
        }
        // Home and office networks are almost always this size.
        let [a, b, c, _] = ip.octets();
        let target = SocketAddr::from((Ipv4Addr::new(a, b, c, 255), port));
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_round_trips() {
        let beacon = Beacon::new([7; 32], 51234, "1|macos|p|Desk".into(), false);
        assert_eq!(Beacon::decode(&beacon.encode()), Some(beacon));
    }

    #[test]
    fn other_packets_are_not_beacons() {
        assert_eq!(Beacon::decode(b""), None);
        assert_eq!(Beacon::decode(b"hello there"), None);
        assert_eq!(Beacon::decode(&[0u8; 2000]), None);
        // Right shape, wrong marker.
        let mut wrong = Beacon::new([1; 32], 1, "x".into(), false);
        wrong.magic = *b"NOPE";
        assert_eq!(Beacon::decode(&wrong.encode()), None);
        // A port of zero cannot be connected to.
        assert_eq!(
            Beacon::decode(&Beacon::new([1; 32], 0, "x".into(), false).encode()),
            None
        );
    }

    #[test]
    fn broadcasts_cover_each_private_network_once() {
        let targets = broadcast_targets(
            [
                Ipv4Addr::new(192, 168, 1, 20),
                Ipv4Addr::new(192, 168, 1, 21),
                Ipv4Addr::new(10, 0, 5, 9),
                Ipv4Addr::new(8, 8, 8, 8),
            ],
            47474,
        );
        let ips: Vec<String> = targets.iter().map(|t| t.ip().to_string()).collect();
        assert_eq!(ips, ["255.255.255.255", "192.168.1.255", "10.0.5.255"]);
    }
}
