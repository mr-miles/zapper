//! The daemon end of a session's NIC: connections and DNS queries captured
//! by the namespace shim (`crate::netns`) arrive here over a unix socket.

use std::net::IpAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use zapper_policy::resolvable;

use super::session::Session;
use super::{dns, proxy, Daemon, EventBody};

pub async fn serve(d: Arc<Daemon>, s: Arc<Session>, listener: UnixListener) {
    let limit = s.runtime.conns();
    super::accept_loop(listener, limit, move |stream| {
        let (d, s) = (d.clone(), s.clone());
        async move {
            let _ = handle(d, s, stream).await;
        }
    })
    .await;
}

async fn handle(d: Arc<Daemon>, s: Arc<Session>, mut stream: UnixStream) -> std::io::Result<()> {
    // The capture shim's one-line command (`TCP <ip> <port>` or `DNS <len>`)
    // fits well under 64 bytes; read that much, then the protocol takes over.
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 64 {
        stream.read_exact(&mut byte).await?;
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    let line = String::from_utf8_lossy(&line).into_owned();
    let parts: Vec<&str> = line.split(' ').collect();
    match parts.as_slice() {
        ["TCP", ip, port] => {
            let (Ok(ip), Ok(port)) = (ip.parse::<IpAddr>(), port.parse::<u16>()) else {
                return Ok(());
            };
            proxy::transparent(d, s, stream, ip, port).await;
        }
        ["DNS", len] => {
            // A DNS query over UDP fits in one ~MTU-sized datagram (1500).
            let len: usize = len.parse().unwrap_or(0);
            if len == 0 || len > 1500 {
                return Ok(());
            }
            let mut query = vec![0u8; len];
            stream.read_exact(&mut query).await?;
            let Some(resp) = query_dns(&d, &s, &query).await else {
                return Ok(());
            };
            stream.write_all(&(resp.len() as u16).to_be_bytes()).await?;
            stream.write_all(&resp).await?;
        }
        _ => {}
    }
    Ok(())
}

fn rcode_name(rcode: u8) -> &'static str {
    match rcode {
        dns::NOERROR => "noerror",
        dns::FORMERR => "formerr",
        dns::SERVFAIL => "servfail",
        dns::NXDOMAIN => "nxdomain",
        dns::NOTIMP => "notimp",
        _ => "refused",
    }
}

/// Answers one DNS query from the guest; `None` if it's too short to answer.
pub(crate) async fn query_dns(d: &Arc<Daemon>, s: &Arc<Session>, query: &[u8]) -> Option<Vec<u8>> {
    let event =
        |name: &str, qtype: u16, rcode: u8, answer: Option<IpAddr>, reason: Option<&str>| {
            d.emit(
                Some(s),
                EventBody::Dns {
                    name: name.to_string(),
                    qtype,
                    allowed: rcode == dns::NOERROR || rcode == dns::NXDOMAIN,
                    rcode: rcode_name(rcode).into(),
                    answer: answer.map(|a| a.to_string()),
                    reason: reason.map(str::to_string),
                },
            )
        };
    let q = match dns::parse(query)? {
        Ok(q) => q,
        Err(rej) => {
            event("", 0, rej.rcode, None, Some(rej.reason));
            return Some(dns::respond(query, None, rej.rcode, None));
        }
    };
    if !s.dns.rate_ok() {
        return Some(dns::respond(query, Some(&q), dns::SERVFAIL, None));
    }
    let (rcode, answer, reason) = resolve_name(d, s, &q).await;
    // Log every refusal and every A/AAAA answer; a NODATA for another record
    // type is not an event.
    if rcode != dns::NOERROR || matches!(q.qtype, dns::TYPE_A | dns::TYPE_AAAA) {
        event(&q.name, q.qtype, rcode, answer, reason.as_deref());
    }
    Some(dns::respond(query, Some(&q), rcode, answer))
}

/// Decides a parsed, rate-passed query: the response code, a synthetic address
/// for an existing name of the queried family, and a reason for the log. Does
/// the host lookup; no event emission or wire encoding.
async fn resolve_name(
    d: &Arc<Daemon>,
    s: &Arc<Session>,
    q: &dns::Question,
) -> (u8, Option<IpAddr>, Option<String>) {
    // `ask` decides access, not resolution: a name some request could reach,
    // with or without an approval, is looked up.
    let reachable = {
        let c = d.config.read().unwrap();
        let layers = s.layers(&c);
        resolvable(&Session::layers_of(&layers), &q.name)
    };
    if !reachable {
        return (dns::REFUSED, None, Some("no rule allows this name".into()));
    }
    // Services and their zones are answered here, never looked up on the host.
    if let Some(exists) = d.services.resolves(s, &q.name) {
        s.dns.names().remember(&q.name, exists);
    }
    // Allowed: say whether the name exists, from a real lookup on the host.
    // Read the cache into a local first so its lock isn't held across `look_up`.
    let cached = s.dns.names().cached(&q.name);
    let exists = match cached {
        Some(e) => e,
        None => match look_up(s, &q.name).await {
            Ok(e) => {
                s.dns.names().remember(&q.name, e);
                e
            }
            Err(why) => return (dns::SERVFAIL, None, Some(why)),
        },
    };
    if !exists {
        return (dns::NXDOMAIN, None, Some("no such name".into()));
    }
    // A synthetic address of the queried family; other types get NODATA.
    let answer = match q.qtype {
        dns::TYPE_A => s.dns.names().address(&q.name).map(IpAddr::V4),
        dns::TYPE_AAAA => s.dns.names().address6(&q.name).map(IpAddr::V6),
        _ => None,
    };
    (dns::NOERROR, answer, None)
}

/// Whether `name` exists, from the host's resolver. `Err` for failures that
/// say nothing about the name, which aren't cached.
async fn look_up(s: &Session, name: &str) -> Result<bool, String> {
    let Some(_permit) = s.dns.lookup_permit() else {
        return Err("too many lookups at once".into());
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::lookup_host((name, 0)),
    )
    .await
    {
        Ok(Ok(mut addrs)) => Ok(addrs.next().is_some()),
        Ok(Err(e)) => {
            let msg = e.to_string();
            // getaddrinfo's "no such name" answers, as opposed to failures.
            if msg.contains("not known")
                || msg.contains("No address associated")
                || msg.contains("Name has no usable address")
            {
                Ok(false)
            } else {
                Err(format!("lookup failed: {msg}"))
            }
        }
        Err(_) => Err("lookup timed out".into()),
    }
}

#[cfg(test)]
mod tests;
