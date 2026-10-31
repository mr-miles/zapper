//! The guest's DNS server.
//!
//! Queries come from the guest and are validated before anything else: one
//! standard query, class IN, a well-formed host name. Answers are never made
//! up, in either direction:
//!
//! - a name some request could reach, allowed outright or after an `ask`, is
//!   looked up on the host; a name that doesn't exist gets NXDOMAIN, one
//!   that does gets a synthetic address standing for it, from 198.18.0.0/15
//!   (A) or 2001:2::/48 (AAAA),
//!   so connections are still decided by name;
//! - any other name gets REFUSED.
//!
//! `ask` decides access, not resolution. A broad `ask` (`*`, or an ask
//! default) therefore lets queries for any name reach the host's resolver.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use zapper_policy::names;

/// A session's DNS state: its synthetic-name table, a query rate limit, and
/// a bound on concurrent host lookups. The single owner of what the session's
/// resolver (`nic`, `proxy`) keeps.
pub(crate) struct SessionDns {
    names: Mutex<FakeDns>,
    rate: Mutex<RateLimit>,
    lookups: Arc<Semaphore>,
}

impl SessionDns {
    pub fn new() -> Self {
        Self {
            names: Mutex::new(FakeDns::default()),
            rate: Mutex::new(RateLimit::default()),
            lookups: Arc::new(Semaphore::new(8)),
        }
    }

    /// A persistent session's: the names it has given out are kept in `path`,
    /// a line each with its address, and given out again from it, so the
    /// addresses the guest has cached mean the same after the daemon
    /// restarts. A line that can't be read changes no other's address.
    pub fn kept(path: &std::path::Path) -> Self {
        let dns = Self::new();
        {
            let mut names = dns.names();
            if let Ok(text) = std::fs::read_to_string(path) {
                for line in text.lines() {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                        continue;
                    };
                    let name = v["name"].as_str().unwrap_or_default();
                    let ip = v["ip"].as_str().and_then(|ip| ip.parse::<IpAddr>().ok());
                    match (v["v"].as_u64(), ip) {
                        (_, Some(ip)) => names.restore(name, ip),
                        // Kept before lines held the address: as given.
                        (Some(4), None) => drop(names.address(name)),
                        (Some(6), None) => drop(names.address6(name)),
                        _ => {}
                    }
                }
            }
            names.log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok();
        }
        dns
    }

    /// The synthetic-name table: names given out by synthetic address, and
    /// what the host's resolver said about each.
    pub fn names(&self) -> MutexGuard<'_, FakeDns> {
        self.names.lock().unwrap()
    }

    /// Whether another query is within the session's rate limit.
    pub fn rate_ok(&self) -> bool {
        self.rate.lock().unwrap().take()
    }

    /// A permit for one concurrent host lookup, or `None` over the cap.
    pub fn lookup_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.lookups.clone().try_acquire_owned().ok()
    }
}

/// The synthetic IPv4 pool, 198.18.0.0/15 (benchmarking, non-routable).
const BASE: u32 = u32::from_be_bytes([198, 18, 0, 0]);
/// The synthetic IPv6 pool, 2001:2::/48 (benchmarking, non-routable).
const BASE6: u128 = Ipv6Addr::new(0x2001, 0x2, 0, 0, 0, 0, 0, 0).to_bits();
const SIZE: u32 = 1 << 17;

/// The address `n` into the v4 or v6 pool.
fn at(v6: bool, n: u128) -> IpAddr {
    if v6 {
        IpAddr::V6(Ipv6Addr::from(BASE6 + n))
    } else {
        IpAddr::V4(Ipv4Addr::from(BASE + n as u32))
    }
}

/// Where `ip` is in its family's pool, if it is: whether v6, and how far
/// in (the pool's base address is none).
fn offset(ip: IpAddr) -> Option<(bool, u128)> {
    let (v6, n) = match ip {
        IpAddr::V4(a) => (false, u32::from(a).wrapping_sub(BASE) as u128),
        IpAddr::V6(a) => (true, a.to_bits().wrapping_sub(BASE6)),
    };
    (n != 0 && n < SIZE as u128).then_some((v6, n))
}
/// How long a lookup's outcome is reused.
const CACHE_TTL: Duration = Duration::from_secs(60);

pub const NOERROR: u8 = 0;
pub(super) const FORMERR: u8 = 1;
pub(super) const SERVFAIL: u8 = 2;
pub(super) const NXDOMAIN: u8 = 3;
pub(super) const NOTIMP: u8 = 4;
pub const REFUSED: u8 = 5;

pub(super) const TYPE_A: u16 = 1;
pub const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

#[derive(Default)]
pub struct FakeDns {
    /// The names given an address of each pool, and theirs.
    v4: HashMap<String, IpAddr>,
    v6: HashMap<String, IpAddr>,
    by_ip: HashMap<IpAddr, String>,
    /// Whether looked-up names exist, and when that was found out.
    exists: HashMap<String, (bool, Instant)>,
    /// Where each name given out is written, for a persistent session.
    log: Option<std::fs::File>,
}

impl FakeDns {
    fn note(&mut self, name: &str, ip: IpAddr) {
        if let Some(f) = self.log.as_mut() {
            use std::io::Write;
            let version = if ip.is_ipv4() { 4 } else { 6 };
            let line = serde_json::json!({ "v": version, "name": name, "ip": ip.to_string() });
            let _ = writeln!(f, "{line}");
        }
    }

    /// `name` at `ip` again, as kept: if `ip` is in its pool and neither it
    /// nor the name has another.
    fn restore(&mut self, name: &str, ip: IpAddr) {
        if name.is_empty() || self.by_ip.contains_key(&ip) {
            return;
        }
        let Some((v6, _)) = offset(ip) else { return };
        if self.pool(v6).contains_key(name) {
            return;
        }
        self.pool(v6).insert(name.to_string(), ip);
        self.by_ip.insert(ip, name.to_string());
    }

    fn pool(&mut self, v6: bool) -> &mut HashMap<String, IpAddr> {
        if v6 {
            &mut self.v6
        } else {
            &mut self.v4
        }
    }

    /// A synthetic address for `name` from the v4 or v6 pool, stable per
    /// name: the next free, past those given and any a kept session's
    /// restore placed further on.
    fn give(&mut self, v6: bool, name: &str) -> Option<IpAddr> {
        if let Some(ip) = self.pool(v6).get(name) {
            return Some(*ip);
        }
        let first = self.pool(v6).len() as u128 + 1;
        let ip = (first..SIZE as u128)
            .map(|n| at(v6, n))
            .find(|ip| !self.by_ip.contains_key(ip))?;
        self.pool(v6).insert(name.to_string(), ip);
        self.by_ip.insert(ip, name.to_string());
        self.note(name, ip);
        Some(ip)
    }

    /// A synthetic IPv4 address for `name`, from the pool, stable per name.
    pub fn address(&mut self, name: &str) -> Option<Ipv4Addr> {
        match self.give(false, name)? {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        }
    }

    /// A synthetic IPv6 address for `name`, from the pool, stable per name.
    pub fn address6(&mut self, name: &str) -> Option<Ipv6Addr> {
        match self.give(true, name)? {
            IpAddr::V6(ip) => Some(ip),
            IpAddr::V4(_) => None,
        }
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
struct RateLimit {
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

    /// Every synthetic address, v4 and v6, is one the address policy keeps
    /// the guest from reaching as a real destination.
    #[test]
    fn the_synthetic_pools_are_unreachable() {
        let policy = super::super::addr::AddressPolicy::new(&[]).unwrap();
        for ip in [
            IpAddr::V4(Ipv4Addr::from(BASE)),
            IpAddr::V4(Ipv4Addr::from(BASE + SIZE - 1)),
            IpAddr::V6(Ipv6Addr::from(BASE6)),
            IpAddr::V6(Ipv6Addr::from(BASE6 + SIZE as u128 - 1)),
        ] {
            assert!(policy.check(ip).is_err(), "{ip}");
        }
    }

    /// Kept names come back at the addresses they had, whatever lines
    /// around them can't be read; a new name gets one no kept name has.
    #[test]
    fn kept_names_keep_their_addresses_past_a_corrupt_line() {
        let dir = std::env::temp_dir().join(format!("zapper-dns-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dns.jsonl");
        let (a, b, c, b6) = {
            let dns = SessionDns::kept(&path);
            let mut n = dns.names();
            (
                n.address("a.example").unwrap(),
                n.address("b.example").unwrap(),
                n.address("c.example").unwrap(),
                n.address6("b.example").unwrap(),
            )
        };
        // The line for a.example torn.
        let text = std::fs::read_to_string(&path).unwrap();
        let torn = text.replacen("{\"ip\"", "{\"ip", 1);
        assert_ne!(torn, text);
        std::fs::write(&path, torn).unwrap();

        let dns = SessionDns::kept(&path);
        let mut n = dns.names();
        assert_eq!(n.address("b.example"), Some(b));
        assert_eq!(n.address("c.example"), Some(c));
        assert_eq!(n.address6("b.example"), Some(b6));
        let fresh = n.address("d.example").unwrap();
        assert!(fresh != b && fresh != c, "{fresh}");
        assert_eq!(n.name(IpAddr::V4(fresh)), Some("d.example"));
        let _ = a;
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    #[test]
    fn concurrent_lookups_are_bounded() {
        let dns = SessionDns::new();
        // Hold every permit; the next lookup is refused until one is freed.
        let held: Vec<_> = std::iter::from_fn(|| dns.lookup_permit()).take(8).collect();
        assert_eq!(held.len(), 8, "eight lookups at once");
        assert!(dns.lookup_permit().is_none(), "the ninth waits");
        drop(held);
        assert!(dns.lookup_permit().is_some(), "freed once one finishes");
    }
}
