use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use super::error::{Error, Result};

/// Resolve `host:port`, but only return addresses that are globally routable.
///
/// Why this exists: without filtering, a hostile DNS record (or an attacker-controlled
/// redirect) can point us at 127.0.0.1, an RFC1918 address, or a link-local address.
/// That turns the browser into a port scanner against the user's own machine and LAN.
/// We resolve manually, inspect every returned IP, and fail closed if any are unsafe.
pub fn resolve(host: &str, port: u16, allow_loopback: bool) -> Result<Vec<SocketAddr>> {
    let iter = (host, port).to_socket_addrs().map_err(Error::Resolve)?;
    let mut out = Vec::new();
    for sa in iter {
        if !is_safe(sa.ip(), allow_loopback) {
            return Err(Error::UnsafeAddress(sa.ip()));
        }
        out.push(sa);
    }
    if out.is_empty() {
        return Err(Error::Resolve(io::Error::new(
            io::ErrorKind::NotFound,
            "no addresses for host",
        )));
    }
    Ok(out)
}

pub fn is_safe(addr: IpAddr, allow_loopback: bool) -> bool {
    if allow_loopback && addr.is_loopback() {
        return true;
    }
    match addr {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(addr: Ipv4Addr) -> bool {
    let [a, b, _, _] = addr.octets();
    !(addr.is_loopback()
        || addr.is_private()
        || addr.is_link_local()
        || addr.is_broadcast()
        || addr.is_documentation()
        || addr.is_unspecified()
        || addr.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || a >= 240)
}

fn is_public_v6(addr: Ipv6Addr) -> bool {
    if let Some(v4) = addr.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segs = addr.segments();
    let is_link_local = (segs[0] & 0xffc0) == 0xfe80;
    let is_unique_local = (segs[0] & 0xfe00) == 0xfc00;
    !(addr.is_loopback()
        || addr.is_unspecified()
        || addr.is_multicast()
        || is_link_local
        || is_unique_local)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }
    fn v6(segs: [u16; 8]) -> IpAddr {
        IpAddr::V6(Ipv6Addr::new(
            segs[0], segs[1], segs[2], segs[3], segs[4], segs[5], segs[6], segs[7],
        ))
    }

    #[test]
    fn rejects_v4_loopback() {
        assert!(!is_safe(v4(127, 0, 0, 1), false));
    }

    #[test]
    fn allows_loopback_when_opted_in() {
        assert!(is_safe(v4(127, 0, 0, 1), true));
    }

    #[test]
    fn rejects_rfc1918() {
        assert!(!is_safe(v4(10, 0, 0, 1), false));
        assert!(!is_safe(v4(172, 16, 0, 1), false));
        assert!(!is_safe(v4(172, 31, 255, 254), false));
        assert!(!is_safe(v4(192, 168, 1, 1), false));
    }

    #[test]
    fn rejects_link_local_v4() {
        assert!(!is_safe(v4(169, 254, 0, 1), false));
    }

    #[test]
    fn rejects_cgnat() {
        assert!(!is_safe(v4(100, 64, 0, 1), false));
        assert!(!is_safe(v4(100, 127, 255, 254), false));
    }

    #[test]
    fn rejects_zero_class() {
        assert!(!is_safe(v4(0, 0, 0, 0), false));
    }

    #[test]
    fn allows_public_v4() {
        assert!(is_safe(v4(1, 1, 1, 1), false));
        assert!(is_safe(v4(8, 8, 8, 8), false));
        assert!(is_safe(v4(99, 0, 0, 1), false));
        assert!(is_safe(v4(101, 0, 0, 1), false));
    }

    #[test]
    fn rejects_v6_loopback() {
        assert!(!is_safe(IpAddr::V6(Ipv6Addr::LOCALHOST), false));
    }

    #[test]
    fn rejects_v6_link_local() {
        assert!(!is_safe(v6([0xfe80, 0, 0, 0, 0, 0, 0, 1]), false));
    }

    #[test]
    fn rejects_v6_unique_local() {
        assert!(!is_safe(v6([0xfc00, 0, 0, 0, 0, 0, 0, 1]), false));
        assert!(!is_safe(v6([0xfd00, 0, 0, 0, 0, 0, 0, 1]), false));
    }

    #[test]
    fn rejects_v4_mapped_v6() {
        // ::ffff:127.0.0.1
        assert!(!is_safe(v6([0, 0, 0, 0, 0, 0xffff, 0x7f00, 1]), false));
    }

    #[test]
    fn allows_public_v6() {
        // 2606:4700::1111 (cloudflare-ish)
        assert!(is_safe(v6([0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111]), false));
    }
}
