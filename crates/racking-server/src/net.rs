//! LAN-only guards (#12): which peer addresses may talk to the server at all,
//! and which addresses it may bind to without the owner saying so.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

/// Peers allowed when the config sets no `allow_from`: loopback and the private
/// and link-local ranges of IPv4 (RFC 1918, RFC 3927) and IPv6 (RFC 4193, RFC 4291).
pub const DEFAULT_ALLOW_FROM: &[&str] = &[
    "127.0.0.0/8",
    "::1/128",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
    "fc00::/7",
    "fe80::/10",
];

/// An address block such as `192.168.0.0/16` or `fe80::/10`; a bare address is a
/// block of one. Host bits past the prefix are ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        // A peer on an IPv6 socket may show as `::ffff:a.b.c.d`; it is the IPv4 peer.
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                same_prefix(u32::from(net).into(), u32::from(ip).into(), 32, self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => same_prefix(u128::from(net), u128::from(ip), 128, self.prefix),
            (IpAddr::V4(_), IpAddr::V6(_)) | (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

/// Whether the top `prefix` bits of two `width`-bit numbers agree.
fn same_prefix(a: u128, b: u128, width: u8, prefix: u8) -> bool {
    let shift = width - prefix;
    shift >= width || (a >> shift) == (b >> shift)
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr
            .parse()
            .map_err(|_| format!("`{s}` is not an IP address or CIDR block"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("`{s}`: the prefix length must be 0 to {max}"))?,
        };
        Ok(Self { addr, prefix })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// Whether `ip` can only be reached from this machine or its local network:
/// loopback, private or link-local. The unspecified address (`0.0.0.0`, `::`)
/// listens on every interface, public ones included, so it does not count.
pub fn is_local(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(s: &str) -> Cidr {
        s.parse().unwrap_or_else(|e: String| panic!("{e}"))
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn ipv4_blocks() {
        let lan = cidr("192.168.0.0/16");
        assert!(lan.contains(ip("192.168.1.20")));
        assert!(!lan.contains(ip("192.169.0.1")));
        assert!(cidr("172.16.0.0/12").contains(ip("172.31.255.255")));
        assert!(!cidr("172.16.0.0/12").contains(ip("172.32.0.1")));
        assert!(cidr("10.1.2.3").contains(ip("10.1.2.3")), "a bare address is a /32");
        assert!(!cidr("10.1.2.3").contains(ip("10.1.2.4")));
        assert!(cidr("0.0.0.0/0").contains(ip("8.8.8.8")));
        assert!(cidr("10.1.2.3/8").contains(ip("10.200.0.1")), "host bits are ignored");
    }

    #[test]
    fn ipv6_blocks() {
        let link = cidr("fe80::/10");
        assert!(link.contains(ip("fe80::1")));
        assert!(link.contains(ip("febf::1")));
        assert!(!link.contains(ip("fec0::1")));
        assert!(cidr("::1").contains(ip("::1")));
        assert!(!cidr("::1").contains(ip("::2")));
        assert!(cidr("fc00::/7").contains(ip("fd12:3456::1")));
        assert!(!cidr("fe80::/10").contains(ip("192.168.0.1")), "families never mix");
        assert!(
            cidr("192.168.0.0/16").contains(ip("::ffff:192.168.0.9")),
            "a mapped IPv4 peer is that peer"
        );
    }

    #[test]
    fn bad_blocks() {
        for bad in ["192.168.0.0/33", "::/129", "10.0.0.0/x", "lan", "10.0.0/8", ""] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad}");
        }
        assert_eq!(cidr("fe80::/10").to_string(), "fe80::/10");
    }

    #[test]
    fn local_addresses() {
        for local in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.20",
            "172.20.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(is_local(ip(local)), "{local}");
        }
        for public in ["0.0.0.0", "::", "8.8.8.8", "2001:db8::1", "172.32.0.1"] {
            assert!(!is_local(ip(public)), "{public}");
        }
    }

    #[test]
    fn defaults_parse() {
        assert!(DEFAULT_ALLOW_FROM.iter().all(|c| c.parse::<Cidr>().is_ok()));
    }
}
