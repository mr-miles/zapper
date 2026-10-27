//! Which destination addresses a session may reach at all.
//!
//! The proxy resolves names on the host and connects to the exact address it
//! checked, so a name cannot be rebound to a forbidden address between the
//! check and the connection.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl std::str::FromStr for Cidr {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let (a, p) = s.split_once('/').unwrap_or((s, ""));
        let addr: IpAddr = a.parse().with_context(|| format!("network {s:?}"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = if p.is_empty() {
            max
        } else {
            p.parse().with_context(|| format!("network {s:?}"))?
        };
        if prefix > max {
            return Err(anyhow!("network {s:?}: prefix too long"));
        }
        Ok(Cidr { addr, prefix })
    }
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix)
                };
                u32::from(n) & mask == u32::from(a) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix)
                };
                u128::from(n) & mask == u128::from(a) & mask
            }
            _ => false,
        }
    }
}

pub struct AddressPolicy {
    pub host_addrs: Vec<IpAddr>,
    pub private_allowed: Vec<Cidr>,
}

fn is_private(ip: IpAddr) -> bool {
    // Parsed once, not per packet. The literals are known-valid, so the
    // `unwrap` runs at first use and never after.
    static RANGES: std::sync::OnceLock<Vec<Cidr>> = std::sync::OnceLock::new();
    RANGES
        .get_or_init(|| {
            [
                "10.0.0.0/8",
                "172.16.0.0/12",
                "192.168.0.0/16",
                "100.64.0.0/10",
                // Benchmarking ranges, also the DNS resolver's synthetic pools:
                // a synthetic address stands for a name within one session and
                // must never be reachable as a real destination (v4
                // `198.18.0.0/15`, v6 `2001:2::/48`, both RFC benchmarking).
                "198.18.0.0/15",
                "2001:2::/48",
                "fc00::/7",
                "fec0::/10",
            ]
            .iter()
            .map(|r| r.parse::<Cidr>().unwrap())
            .collect()
        })
        .iter()
        .any(|c| c.contains(ip))
}

impl AddressPolicy {
    pub fn new(private_networks: &[String]) -> Result<Self> {
        let private_allowed = private_networks
            .iter()
            .map(|n| n.parse())
            .collect::<Result<_>>()?;
        Ok(Self {
            host_addrs: host_addrs(),
            private_allowed,
        })
    }

    /// Why `ip` may not be reached, if it may not.
    pub fn check(&self, ip: IpAddr) -> Result<(), &'static str> {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        let special = match ip {
            IpAddr::V4(a) => {
                a.is_loopback()
                    || a.is_unspecified()
                    || a.is_link_local()
                    || a.is_multicast()
                    || a.is_broadcast()
                    || a.octets()[0] == 0
            }
            IpAddr::V6(a) => {
                a.is_loopback()
                    || a.is_unspecified()
                    || a.is_multicast()
                    || (a.segments()[0] & 0xffc0) == 0xfe80
            }
        };
        if special {
            return Err("loopback, link-local or special address");
        }
        if self.host_addrs.contains(&ip) {
            return Err("address of the zapper host");
        }
        if is_private(ip) && !self.private_allowed.iter().any(|c| c.contains(ip)) {
            return Err("private address not in daemon.private_networks");
        }
        Ok(())
    }
}

/// Addresses of the host's interfaces.
fn host_addrs() -> Vec<IpAddr> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs allocates a list we walk via ifa_next until null and
    // free with freeifaddrs; each node's ifa_addr is checked for null and its
    // sa_family read before casting to the matching sockaddr_in/in6.
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut p = ifap;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() {
                match (*a).sa_family as i32 {
                    libc::AF_INET => {
                        let sin = &*(a as *const libc::sockaddr_in);
                        out.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                            sin.sin_addr.s_addr,
                        ))));
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*(a as *const libc::sockaddr_in6);
                        out.push(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks() {
        let p = AddressPolicy {
            host_addrs: vec!["192.0.2.7".parse().unwrap()],
            private_allowed: vec!["10.107.111.0/24".parse().unwrap()],
        };
        let check = |s: &str| p.check(s.parse().unwrap());
        assert!(check("1.1.1.1").is_ok());
        assert!(check("127.0.0.1").is_err());
        assert!(check("169.254.169.254").is_err());
        assert!(check("::ffff:127.0.0.1").is_err());
        assert!(check("fe80::1").is_err());
        assert!(check("192.0.2.7").is_err());
        assert!(check("10.1.2.3").is_err());
        assert!(check("10.107.111.20").is_ok());
        assert!(check("2606:4700::1111").is_ok());
        assert!(check("fd00::1").is_err());
        // The synthetic DNS pools are never reachable as real destinations,
        // v6 (2001:2::/48) like v4 (198.18.0.0/15).
        assert!(check("198.18.0.1").is_err());
        assert!(check("2001:2::1").is_err());
        // A different 2001:... address is ordinary public space.
        assert!(check("2001:4860:4860::8888").is_ok());
    }

    /// A synthetic pool stays blocked even if someone lists it in
    /// `private_networks`? No — like the v4 benchmarking range, it is
    /// overridable; it just never routes anywhere. What matters is the
    /// default-closed behaviour above.
    #[test]
    fn synthetic_pools_are_private_by_default() {
        let p = AddressPolicy {
            host_addrs: vec![],
            private_allowed: vec![],
        };
        assert!(p.check("198.18.7.7".parse().unwrap()).is_err());
        assert!(p.check("2001:2::dead".parse().unwrap()).is_err());
    }
}
