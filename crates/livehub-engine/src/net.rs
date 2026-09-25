//! Dual-stack UDP plumbing. One socket family for every send/receive path so the
//! phone link works on BOTH IPv4 LANs and IPv6-only networks (a carrier hotspot in
//! 464XLAT mode advertises a dead 172.20.10.1 and routes only IPv6 — measured
//! 2026-09-24 on a T-Mobile iPhone hotspot).
//!
//! Measured on this Mac (macOS 26): a `[::]` UDP socket receives IPv4 datagrams as
//! v4-mapped (`::ffff:a.b.c.d`) and SENDS to a v4-mapped destination fine, but
//! sending to a plain `SocketAddr::V4` from it fails with EINVAL — hence
//! [`dual_dest`] on every send.

use std::io;
use std::net::{SocketAddr, SocketAddrV6, UdpSocket};

/// Bind a dual-stack UDP socket (`[::]:port`; port 0 = ephemeral). Handles both
/// IPv6 peers and IPv4 peers (as v4-mapped addresses). On a Mac with IPv6
/// disabled (`networksetup -setv6off` — a common WiFi-troubleshooting step) the
/// v6 bind fails, so fall back to plain IPv4: v4 peers keep working, and the v6
/// candidates were unreachable on such a machine anyway.
pub fn udp_dual(port: u16) -> io::Result<UdpSocket> {
    UdpSocket::bind(("::", port)).or_else(|_| UdpSocket::bind(("0.0.0.0", port)))
}

/// The destination `sock` can actually send `addr` to: v4-mapped for a
/// dual-stack v6 socket, the plain address when the socket fell back to IPv4
/// (a mapped dest is EINVAL from a v4 socket; a v6 dest then fails naturally,
/// and beam's candidate rotation moves past it).
pub fn sock_dest(sock: &UdpSocket, addr: SocketAddr) -> SocketAddr {
    match sock.local_addr() {
        Ok(local) if local.is_ipv4() => addr,
        _ => dual_dest(addr),
    }
}

/// A destination usable from a [`udp_dual`] socket: IPv4 becomes v4-mapped IPv6
/// (`::ffff:a.b.c.d`), IPv6 passes through (scope_id preserved).
pub fn dual_dest(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) => {
            SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
        }
        v6 => v6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_socket_reaches_a_plain_v4_receiver() {
        let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
        recv.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let to = recv.local_addr().unwrap();

        let s = udp_dual(0).unwrap();
        // The raw V4 addr must be mapped first — a plain V4 dest is EINVAL from [::].
        assert!(s.send_to(b"x", to).is_err(), "unmapped v4 send should fail from [::]");
        s.send_to(b"hi", dual_dest(to)).unwrap();
        let mut buf = [0u8; 8];
        let (n, _) = recv.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hi");
    }

    #[test]
    fn dual_socket_receives_from_a_plain_v4_sender() {
        let recv = udp_dual(0).unwrap();
        recv.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let port = recv.local_addr().unwrap().port();
        let send = UdpSocket::bind("0.0.0.0:0").unwrap();
        send.send_to(b"hi", format!("127.0.0.1:{port}")).unwrap();
        let mut buf = [0u8; 8];
        let (n, from) = recv.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hi");
        // Arrives as v4-mapped — proves one [::] socket serves both families.
        assert!(from.ip().to_string().starts_with("::ffff:"), "got {from}");
    }

    #[test]
    fn sock_dest_keeps_plain_v4_for_a_v4_socket() {
        // The IPv4 fallback path: a v4-bound socket must get the PLAIN v4 dest
        // (a mapped one is EINVAL from it), a v6 socket the mapped form.
        let v4 = UdpSocket::bind("0.0.0.0:0").unwrap();
        let dest: SocketAddr = "192.0.2.1:9000".parse().unwrap();
        assert_eq!(sock_dest(&v4, dest), dest);
        let v6 = UdpSocket::bind("[::]:0").unwrap();
        assert_eq!(sock_dest(&v6, dest), dual_dest(dest));
    }

    #[test]
    fn dual_dest_preserves_v6_scope_id() {
        let a: SocketAddr = "[fe80::1%14]:9000".parse().unwrap();
        assert_eq!(dual_dest(a), a);
        match dual_dest(a) {
            SocketAddr::V6(v6) => assert_eq!(v6.scope_id(), 14),
            _ => panic!("v6 in, v6 out"),
        }
    }
}
