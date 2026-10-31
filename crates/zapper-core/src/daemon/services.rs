//! Services: names sessions use that the daemon connects itself, never
//! looked up on the host. A service maps a name and port to an in-process
//! handler, a unix socket a program on the host serves (told which session
//! is calling), an address on the host, or a port of another session's VM,
//! for the sessions its label selector picks. A zone is a domain whose names
//! resolve only to services, for the sessions it is for: a name in it that
//! nothing serves doesn't exist. Reaching a service still needs a net rule.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};

use super::registry::{Entry, Registry};
use super::session::Session;

/// A stream to or from a service.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// An in-process service: serves each connection a session opens to it.
pub trait Handler: Send + Sync {
    /// Serves one connection from `session`: the plain stream when the
    /// service terminates TLS, else as the guest sent it.
    fn serve(&self, session: Arc<Session>, io: Box<dyn Io>);
}

/// Where a service's connections go.
#[derive(Clone)]
pub enum Target {
    /// Served in the daemon. With `tls`, the daemon terminates TLS with the
    /// session's CA for the name asked for, offering HTTP/1.1 only (ALPN).
    Handler {
        tls: bool,
        handler: Arc<dyn Handler>,
    },
    /// An address on the host's network, still subject to the address
    /// policy.
    Addr(SocketAddr),
    /// A unix socket in the daemon's `state/services/` directory, served by
    /// a program on the host (a facility outside the daemon). Each connection
    /// starts with one line from the daemon naming the caller ([`Caller`]),
    /// then carries the guest's stream. With `tls`, the daemon terminates
    /// TLS as for a handler, and the socket gets the plain stream.
    Socket { name: String, tls: bool },
    /// A port of another session's VM, over vsock.
    Session { id: String, port: u32 },
}

/// The directory under `state` holding the sockets socket services name.
pub const SOCKETS_DIR: &str = "services";

/// The first line on a connection to a socket service: who is calling,
/// from the VM the connection came from. The guest's own bytes follow it,
/// so nothing the guest sends can pose as it.
#[derive(Debug, Serialize, serde::Deserialize, PartialEq)]
pub struct Caller {
    pub session: String,
    pub labels: BTreeMap<String, String>,
    /// The name the guest asked for, and the port.
    pub name: String,
    pub port: u16,
}

/// `name` as a socket's file name in `state/services/`: letters, digits,
/// `.`, `-` and `_`, not starting with `.`, so it can't name anything
/// outside that directory.
fn check_socket(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if !ok {
        bail!(
            "{name:?} is not a socket name: a file name in the daemon's {SOCKETS_DIR}/ directory \
             (letters, digits, '.', '-', '_'; at most 64)"
        );
    }
    Ok(())
}

/// A service to register.
#[derive(Clone)]
pub struct Spec {
    /// The name sessions use; `*.` in front stands for any name under it.
    pub name: String,
    pub port: u16,
    /// The sessions that may use it: those with all these labels; every
    /// session when empty.
    pub sessions: BTreeMap<String, String>,
    pub target: Target,
    /// Who registered it (`operator` through the API), shown in the list.
    pub owner: String,
}

/// A registered service or zone, for the list.
#[derive(Debug, Clone, Serialize)]
pub struct Info {
    pub id: u64,
    pub kind: &'static str,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub sessions: BTreeMap<String, String>,
    pub to: String,
    pub owner: String,
}

enum Item {
    Service {
        name: String,
        port: u16,
        target: Target,
        owner: String,
    },
    Zone {
        domain: String,
        owner: String,
    },
}

/// The daemon's services and zones.
#[derive(Default)]
pub struct Services(Registry<Item>);

/// Where `host:port` goes for a session.
pub enum Route {
    /// To this service.
    Service(Target),
    /// Nowhere: the name is a service's or in a zone, but nothing serves
    /// this port of it.
    Unserved,
}

/// `name` as a service name: a host name, or `*.` and one.
fn check_name(name: &str) -> Result<String> {
    let (wild, rest) = match name.strip_prefix("*.") {
        Some(r) => (true, r),
        None => (false, name),
    };
    let host = zapper_policy::names::hostname(rest)
        .map_err(|e| anyhow!("{name:?} is not a service name: {e}"))?;
    Ok(if wild { format!("*.{host}") } else { host })
}

fn matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(base) => host.len() > base.len() + 1 && host.ends_with(&format!(".{base}")),
        None => pattern == host,
    }
}

fn in_zone(domain: &str, host: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn describe(t: &Target) -> String {
    match t {
        Target::Handler { tls: true, .. } => "in the daemon (TLS terminated)".into(),
        Target::Handler { tls: false, .. } => "in the daemon".into(),
        Target::Socket { name, tls: true } => format!("socket {name} (TLS terminated)"),
        Target::Socket { name, tls: false } => format!("socket {name}"),
        Target::Addr(a) => a.to_string(),
        Target::Session { id, port } => format!("session {id} port {port}"),
    }
}

impl Services {
    pub fn add(&self, spec: Spec) -> Result<u64> {
        let Spec {
            name,
            port,
            sessions,
            target,
            owner,
        } = spec;
        let name = check_name(&name)?;
        if port == 0 {
            bail!("a service needs a port");
        }
        if let Target::Socket { name, .. } = &target {
            check_socket(name)?;
        }
        let item = Item::Service {
            name: name.clone(),
            port,
            target,
            owner,
        };
        self.0
            .add_unless(sessions.clone(), item, |e| {
                e.sessions == sessions
                    && matches!(&e.item, Item::Service { name: n, port: p, .. } if *n == name && *p == port)
            })
            .ok_or_else(|| anyhow!("{name}:{port} is already a service for these sessions"))
    }

    pub fn add_zone(
        &self,
        domain: &str,
        sessions: BTreeMap<String, String>,
        owner: &str,
    ) -> Result<u64> {
        let domain = zapper_policy::names::hostname(domain)
            .map_err(|e| anyhow!("{domain:?} is not a domain: {e}"))?;
        let owner = owner.to_string();
        Ok(self.0.add(sessions, Item::Zone { domain, owner }))
    }

    /// Removes a service or zone; what it was, if it was there.
    pub fn remove(&self, id: u64) -> Option<Info> {
        self.0.remove(id).map(|e| info(id, &e))
    }

    pub fn info(&self, id: u64) -> Option<Info> {
        self.list().into_iter().find(|i| i.id == id)
    }

    pub fn list(&self) -> Vec<Info> {
        self.0
            .with(|entries| entries.iter().map(|(id, e)| info(*id, e)).collect())
    }

    /// Where `host:port` goes for `s`: `None` for a name that is no
    /// service's and in no zone, which the host resolves. An exact name
    /// wins over `*.`, then the earliest registered.
    pub fn route(&self, s: &Session, host: &str, port: u16) -> Option<Route> {
        self.0.with(|entries| {
            let mut known = false;
            let mut best: Option<(bool, &Target)> = None;
            for e in entries.values().filter(|e| e.picks(s)) {
                match &e.item {
                    Item::Service {
                        name,
                        port: p,
                        target,
                        ..
                    } if matches(name, host) => {
                        known = true;
                        let exact = !name.starts_with("*.");
                        if *p == port && best.is_none_or(|(e, _)| exact && !e) {
                            best = Some((exact, target));
                        }
                    }
                    Item::Zone { domain, .. } if in_zone(domain, host) => known = true,
                    _ => {}
                }
            }
            match best {
                Some((_, target)) => Some(Route::Service(target.clone())),
                None => known.then_some(Route::Unserved),
            }
        })
    }

    /// Whether `name` exists for `s` without asking the host: `None` when
    /// the host should say.
    pub fn resolves(&self, s: &Session, name: &str) -> Option<bool> {
        self.0.with(|entries| {
            let mut zoned = false;
            for e in entries.values().filter(|e| e.picks(s)) {
                match &e.item {
                    Item::Service { name: n, .. } if matches(n, name) => return Some(true),
                    Item::Zone { domain, .. } if in_zone(domain, name) => zoned = true,
                    _ => {}
                }
            }
            zoned.then_some(false)
        })
    }
}

fn info(id: u64, e: &Entry<Item>) -> Info {
    let sessions = e.sessions.clone();
    match &e.item {
        Item::Service {
            name,
            port,
            target,
            owner,
        } => Info {
            id,
            kind: "service",
            name: name.clone(),
            port: Some(*port),
            sessions,
            to: describe(target),
            owner: owner.clone(),
        },
        Item::Zone { domain, owner } => Info {
            id,
            kind: "zone",
            name: domain.clone(),
            port: None,
            sessions,
            to: "nothing but its services".into(),
            owner: owner.clone(),
        },
    }
}

/// Opens a connection from `s` to a service's target. `host` and `port` are
/// what it asked for: a TLS-terminating target presents a certificate for
/// `host`, and a socket service is told both.
pub async fn connect(
    d: &super::Daemon,
    s: &Arc<Session>,
    host: &str,
    port: u16,
    target: &Target,
) -> Result<Box<dyn Io>> {
    match target {
        Target::Handler {
            tls: false,
            handler,
        } => Ok(serve_handler(s, handler)),
        Target::Handler { tls: true, handler } => {
            let (s2, handler) = (s.clone(), handler.clone());
            behind_tls(s, host, move |tls| handler.serve(s2, Box::new(tls)))
        }
        Target::Socket { name, tls: false } => {
            Ok(Box::new(open_socket(d, s, host, port, name).await?))
        }
        Target::Socket { name, tls: true } => {
            let mut sock = open_socket(d, s, host, port, name).await?;
            behind_tls(s, host, move |mut tls| {
                tokio::spawn(async move {
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut sock).await;
                });
            })
        }
        Target::Addr(addr) => {
            let checked = d
                .addrs
                .read()
                .unwrap()
                .checked(*addr)
                .map_err(|e| anyhow!("{addr}: {e}"))?;
            let tcp = super::proxy::dial(checked)
                .await
                .with_context(|| format!("connecting to {addr}"))?;
            Ok(Box::new(tcp))
        }
        Target::Session { id, port } => {
            let other = d
                .session(id)
                .map_err(|_| anyhow!("session {id}, which serves {host}, is gone"))?;
            let stream = other
                .connect_port(*port)
                .await
                .with_context(|| format!("session {id}, which serves {host}"))?;
            Ok(Box::new(stream))
        }
    }
}

/// The plain side of a target whose TLS the daemon terminates (a handler
/// or a socket with `tls`): what it is given once TLS is off. For the
/// inspecting proxy, which terminates the guest's TLS itself and decides
/// each request before passing it on. `None` for other targets.
pub async fn connect_plain(
    d: &super::Daemon,
    s: &Arc<Session>,
    host: &str,
    port: u16,
    target: &Target,
) -> Result<Option<Box<dyn Io>>> {
    match target {
        Target::Handler { tls: true, handler } => Ok(Some(serve_handler(s, handler))),
        Target::Socket { name, tls: true } => {
            Ok(Some(Box::new(open_socket(d, s, host, port, name).await?)))
        }
        _ => Ok(None),
    }
}

/// A stream to `handler`, serving `s`.
fn serve_handler(s: &Arc<Session>, handler: &Arc<dyn Handler>) -> Box<dyn Io> {
    let (a, b) = tokio::io::duplex(256 * 1024);
    handler.serve(s.clone(), Box::new(b));
    Box::new(a)
}

/// Connects to socket `name` in `state/services/` and sends the caller line.
async fn open_socket(
    d: &super::Daemon,
    s: &Arc<Session>,
    host: &str,
    port: u16,
    name: &str,
) -> Result<tokio::net::UnixStream> {
    let path = d.paths.state.join(SOCKETS_DIR).join(name);
    let mut sock = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::UnixStream::connect(&path),
    )
    .await
    .map_err(|_| anyhow!("connecting to socket {name}: timed out"))?
    .with_context(|| format!("connecting to socket {name}, which serves {host}"))?;
    let caller = Caller {
        session: s.id.clone(),
        labels: s.labels.clone(),
        name: host.to_string(),
        port,
    };
    let mut line = serde_json::to_vec(&caller)?;
    line.push(b'\n');
    tokio::io::AsyncWriteExt::write_all(&mut sock, &line).await?;
    Ok(sock)
}

/// A stream the guest's TLS ends on, with the session's CA for `host`;
/// `serve` gets the plain stream once the handshake is done. HTTP/1.1 only:
/// what's behind gets a byte stream and speaks HTTP/1.1 if it speaks HTTP
/// (the firm's git server does), so a client that would rather have HTTP/2
/// (git's curl) doesn't pick a protocol nobody on this side speaks.
fn behind_tls<F>(s: &Arc<Session>, host: &str, serve: F) -> Result<Box<dyn Io>>
where
    F: FnOnce(tokio_rustls::server::TlsStream<tokio::io::DuplexStream>) + Send + 'static,
{
    let (a, b) = tokio::io::duplex(256 * 1024);
    let mut config = (*s.tls.server_config(host)?).clone();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let config = Arc::new(config);
    tokio::spawn(async move {
        let Ok(Ok(tls)) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_rustls::TlsAcceptor::from(config).accept(b),
        )
        .await
        else {
            return;
        };
        serve(tls);
    });
    Ok(Box::new(a))
}

#[cfg(test)]
mod tests;
