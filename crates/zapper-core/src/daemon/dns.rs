//! The guest's DNS server.
//!
//! Queries come from the guest and are validated before anything else: one
//! standard query, class IN, a well-formed host name. Answers are never made
//! up, in either direction:
//!
//! - a name some request could reach, allowed outright or after an `ask`, is
//!   looked up on the host; a name that doesn't exist gets NXDOMAIN, one
//!   that does gets a synthetic address from 198.18.0.0/15 standing for it,
//!   so connections are still decided by name;
//! - any other name gets REFUSED.
//!
//! `ask` decides access, not resolution. A broad `ask` (`*`, or an ask
//! default) therefore lets queries for any name reach the host's resolver.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use zapper_policy::names;

/// The synthetic IPv4 pool, 198.18.0.0/15 (benchmarking, non-routable).
const BASE: u32 = u32::from_be_bytes([198, 18, 0, 0]);
/// The synthetic IPv6 pool, 2001:2::/48 (benchmarking, non-routable).
const BASE6: u128 = Ipv6Addr::new(0x2001, 0x2, 0, 0, 0, 0, 0, 0).to_bits();
const SIZE: u32 = 1 << 17;
/// How long a lookup's outcome is reused.
const CACHE_TTL: Duration = Duration::from_secs(60);

pub const NOERROR: u8 = 0;
pub const FORMERR: u8 = 1;
pub const SERVFAIL: u8 = 2;
pub const NXDOMAIN: u8 = 3;
pub const NOTIMP: u8 = 4;
pub const REFUSED: u8 = 5;

pub const TYPE_A: u16 = 1;
pub const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

#[derive(Default)]
pub struct FakeDns {
    v4: HashMap<String, Ipv4Addr>,
    v6: HashMap<String, Ipv6Addr>,
    by_ip: HashMap<IpAddr, String>,
    /// Whether looked-up names exist, and when that was found out.
    exists: HashMap<String, (bool, Instant)>,
}

impl FakeDns {
    /// A synthetic IPv4 address for `name`, from the pool, stable per name.
    pub fn address(&mut self, name: &str) -> Option<Ipv4Addr> {
        if let Some(ip) = self.v4.get(name) {
            return Some(*ip);
        }
        let n = self.v4.len() as u32 + 1;
        if n >= SIZE {
            return None;
        }
        let ip = Ipv4Addr::from(BASE + n);
        self.v4.insert(name.to_string(), ip);
        self.by_ip.insert(IpAddr::V4(ip), name.to_string());
        Some(ip)
    }

    /// A synthetic IPv6 address for `name`, from the pool, stable per name.
    pub fn address6(&mut self, name: &str) -> Option<Ipv6Addr> {
        if let Some(ip) = self.v6.get(name) {
            return Some(*ip);
        }
        let n = self.v6.len() as u128 + 1;
        if n >= SIZE as u128 {
            return None;
        }
        let ip = Ipv6Addr::from(BASE6 + n);
        self.v6.insert(name.to_string(), ip);
        self.by_ip.insert(IpAddr::V6(ip), name.to_string());
        Some(ip)
    }

    pub fn name(&self, ip: IpAddr) -> Option<&str> {
        self.by_ip.get(&ip).map(String::as_str)
    }

    pub fn cached(&self, name: &str) -> Option<bool> {
        self.exists
            .get(name)
            .filter(|(_, at)| at.elapsed() < CACHE_TTL)
            .map(|(e, _)| *e)
    }

    pub fn remember(&mut self, name: &str, exists: bool) {
        if self.exists.len() > 10_000 {
            self.exists.retain(|_, (_, at)| at.elapsed() < CACHE_TTL);
        }
        self.exists
            .insert(name.to_string(), (exists, Instant::now()));
    }
}

#[derive(Debug)]
pub struct Question {
    pub name: String,
    pub qtype: u16,
    /// End of the question section in the query.
    end: usize,
}

/// A query rejected before policy, with the code to answer and why.
#[derive(Debug, PartialEq)]
pub struct Rejected {
    pub rcode: u8,
    pub reason: &'static str,
}

fn reject(rcode: u8, reason: &'static str) -> Rejected {
    Rejected { rcode, reason }
}

/// Validates a query and reads its question. `None`: too short to answer.
pub fn parse(q: &[u8]) -> Option<Result<Question, Rejected>> {
    if q.len() < 12 {
        return None;
    }
    let count = |i: usize| u16::from_be_bytes([q[i], q[i + 1]]);
    Some((|| {
        if q[2] & 0x80 != 0 {
            return Err(reject(FORMERR, "a response, not a query"));
        }
        if (q[2] >> 3) & 0x0f != 0 {
            return Err(reject(NOTIMP, "not a standard query"));
        }
        if count(4) != 1 {
            return Err(reject(FORMERR, "not exactly one question"));
        }
        if count(6) != 0 || count(8) != 0 {
            return Err(reject(FORMERR, "answer or authority records in a query"));
        }
        let mut i = 12;
        let mut labels = Vec::new();
        loop {
            let len = *q.get(i).ok_or(reject(FORMERR, "truncated name"))? as usize;
            i += 1;
            if len == 0 {
                break;
            }
            // The top two bits mark compression pointers and extended labels,
            // which have no place in a question.
            if len & 0xc0 != 0 {
                return Err(reject(FORMERR, "compressed or extended label"));
            }
            let l = q.get(i..i + len).ok_or(reject(FORMERR, "truncated name"))?;
            names::label(l).map_err(|r| reject(REFUSED, r))?;
            labels.push(std::str::from_utf8(l).expect("validated ASCII"));
            i += len;
        }
        let tail = q
            .get(i..i + 4)
            .ok_or(reject(FORMERR, "truncated question"))?;
        let qtype = u16::from_be_bytes([tail[0], tail[1]]);
        let qclass = u16::from_be_bytes([tail[2], tail[3]]);
        if qclass != CLASS_IN {
            return Err(reject(REFUSED, "class other than IN"));
        }
        let name = names::hostname(&labels.join(".")).map_err(|r| reject(REFUSED, r))?;
        Ok(Question {
            name,
            qtype,
            end: i + 4,
        })
    })())
}

/// A response to `query` with `rcode`, echoing the question if there is a
/// valid one, and an address record if `answer` is set: an A record for an
/// IPv4 answer, AAAA for IPv6.
pub fn respond(
    query: &[u8],
    question: Option<&Question>,
    rcode: u8,
    answer: Option<IpAddr>,
) -> Vec<u8> {
    let mut r = query[..12].to_vec();
    r[2] = 0x80 | (query[2] & 0x79); // QR, opcode, RD
    r[3] = 0x80 | (rcode & 0x0f); // RA, rcode
    r[4..12].fill(0);
    if let Some(q) = question {
        r[5] = 1;
        r.extend_from_slice(&query[12..q.end]);
        if let Some(ip) = answer {
            r[7] = 1;
            r.extend_from_slice(&[0xc0, 0x0c]); // name: pointer to the question
            let (rtype, rdata): (u16, Vec<u8>) = match ip {
                IpAddr::V4(a) => (TYPE_A, a.octets().to_vec()),
                IpAddr::V6(a) => (TYPE_AAAA, a.octets().to_vec()),
            };
            r.extend_from_slice(&rtype.to_be_bytes());
            r.extend_from_slice(&CLASS_IN.to_be_bytes());
            r.extend_from_slice(&60u32.to_be_bytes()); // TTL
            r.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            r.extend_from_slice(&rdata);
        }
    }
    r
}

/// Per-session query budget: `RATE` per second, bursts of `BURST`.
pub struct RateLimit {
    tokens: f64,
    at: Instant,
}

const RATE: f64 = 50.0;
const BURST: f64 = 100.0;

impl Default for RateLimit {
    fn default() -> Self {
        RateLimit {
            tokens: BURST,
            at: Instant::now(),
        }
    }
}

impl RateLimit {
    pub fn take(&mut self) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.at).as_secs_f64() * RATE).min(BURST);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(labels: &[&[u8]], qtype: u16, qclass: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for l in labels {
            q.push(l.len() as u8);
            q.extend_from_slice(l);
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&qclass.to_be_bytes());
        q
    }

    fn rejected(q: &[u8]) -> Rejected {
        parse(q).unwrap().unwrap_err()
    }

    #[test]
    fn parses_valid() {
        let q = query(&[b"API", b"Example", b"com"], TYPE_A, 1);
        let p = parse(&q).unwrap().unwrap();
        assert_eq!((p.name.as_str(), p.qtype), ("api.example.com", TYPE_A));
        let r = respond(
            &q,
            Some(&p),
            NOERROR,
            Some(IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1))),
        );
        assert_eq!(&r[..2], &[0x12, 0x34]);
        assert_eq!((r[3] & 0x0f, r[5], r[7]), (0, 1, 1));
        // An A record (12-byte record + 4-byte rdata): type A, then the address.
        assert_eq!(
            u16::from_be_bytes([r[r.len() - 14], r[r.len() - 13]]),
            TYPE_A
        );
        assert_eq!(&r[r.len() - 4..], &[198, 18, 0, 1]);
    }

    #[test]
    fn answers_aaaa_with_a_v6_record() {
        let q = query(&[b"api", b"example"], TYPE_AAAA, 1);
        let p = parse(&q).unwrap().unwrap();
        assert_eq!(p.qtype, TYPE_AAAA);
        let v6 = Ipv6Addr::new(0x2001, 0x2, 0, 0, 0, 0, 0, 1);
        let r = respond(&q, Some(&p), NOERROR, Some(IpAddr::V6(v6)));
        assert_eq!((r[3] & 0x0f, r[5], r[7]), (0, 1, 1));
        // An AAAA record (12-byte record + 16-byte rdata): type AAAA, then it.
        assert_eq!(
            u16::from_be_bytes([r[r.len() - 26], r[r.len() - 25]]),
            TYPE_AAAA
        );
        assert_eq!(&r[r.len() - 16..], &v6.octets());
    }

    #[test]
    fn rejects_bad_queries() {
        assert!(parse(&[0; 5]).is_none());
        let mut resp = query(&[b"a", b"com"], 1, 1);
        resp[2] |= 0x80;
        assert_eq!(rejected(&resp).rcode, FORMERR);
        let mut update = query(&[b"a", b"com"], 1, 1);
        update[2] = 5 << 3;
        assert_eq!(rejected(&update).rcode, NOTIMP);
        let mut two = query(&[b"a", b"com"], 1, 1);
        two[5] = 2;
        assert_eq!(rejected(&two).rcode, FORMERR);
        assert_eq!(
            rejected(&query(&[b"a.b", b"com"], 1, 1)).rcode,
            REFUSED,
            "dot inside a label"
        );
        assert_eq!(
            rejected(&query(&[b"a b", b"com"], 1, 1)).rcode,
            REFUSED,
            "space"
        );
        assert_eq!(
            rejected(&query(&[b"a\0", b"com"], 1, 1)).rcode,
            REFUSED,
            "NUL"
        );
        assert_eq!(rejected(&query(&[b"-a", b"com"], 1, 1)).rcode, REFUSED);
        assert_eq!(rejected(&query(&[], 1, 1)).rcode, REFUSED, "root");
        assert_eq!(
            rejected(&query(&[b"a", b"com"], 1, 3)).rcode,
            REFUSED,
            "class CHAOS"
        );
        let mut ptr = query(&[b"a"], 1, 1);
        ptr[12] = 0xc0;
        assert_eq!(rejected(&ptr).rcode, FORMERR, "compression pointer");
        let truncated = &query(&[b"abc", b"com"], 1, 1)[..15];
        assert_eq!(rejected(truncated).rcode, FORMERR);
        // A refusal for a malformed query carries no question.
        let r = respond(&ptr, None, FORMERR, None);
        assert_eq!((r.len(), r[3] & 0x0f, r[5]), (12, FORMERR, 0));
    }

    #[test]
    fn synthetic_addresses_and_cache() {
        let mut f = FakeDns::default();
        let ip = f.address("a.example").unwrap();
        assert_eq!(f.address("a.example"), Some(ip));
        assert_eq!(f.name(IpAddr::V4(ip)), Some("a.example"));
        // A v6 synthetic is separate, stable, and reverse-maps too.
        let ip6 = f.address6("a.example").unwrap();
        assert!(Ipv6Addr::from(BASE6).octets()[..6] == ip6.octets()[..6]);
        assert_eq!(f.address6("a.example"), Some(ip6));
        assert_eq!(f.name(IpAddr::V6(ip6)), Some("a.example"));
        // The two families don't collide in the reverse map.
        assert_eq!(f.name(IpAddr::V4(ip)), Some("a.example"));
        assert_eq!(f.cached("a.example"), None);
        f.remember("a.example", false);
        assert_eq!(f.cached("a.example"), Some(false));
    }

    #[test]
    fn rate_limit() {
        let mut r = RateLimit::default();
        assert!((0..100).all(|_| r.take()));
        assert!(!r.take());
    }
}
