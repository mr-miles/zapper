//! The egress proxy: the only way out of a session.
//!
//! The guest speaks plain HTTP proxy protocol over vsock. `CONNECT` tunnels
//! are either passed through after an endpoint check, with the TLS server
//! name bound to the `CONNECT` host, or, when a rule needs to see requests,
//! terminated with the session CA so every request is checked on its own and
//! secrets can be injected. Plain `http://` requests are checked per request;
//! a secret is injected into one only if it is marked `plaintext: true`, since
//! anything on the path can read a plaintext request — a secret without that
//! flag is refused rather than sent in the clear.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue, HOST};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpStream, UnixStream};
use zapper_policy::{
    evaluate_net, needs_inspection, placeholder, render, resolvable, Action, AnyRule, Decision,
    Kind, NetEffects, NetRequest, NetRule,
};

use super::deciders::Referral;
use super::session::Session;
use super::{addr, httplog, services, tls, Daemon, EventBody};

mod inspect;
// Re-exported for the fuzz targets (crate::fuzz), which exercise these parsers.
#[cfg(feature = "fuzz")]
pub(crate) use inspect::{clean_headers, inject_with, upstream_uri};
mod aws;
mod relay;
mod sanitize;

type Body = BoxBody<Bytes, hyper::Error>;

fn text(status: StatusCode, msg: impl Into<String>) -> Response<Body> {
    let body = Full::new(Bytes::from(format!("zapper: {}\n", msg.into())))
        .map_err(|n: Infallible| match n {})
        .boxed();
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(body)
        .unwrap()
}

fn http1() -> hyper::server::conn::http1::Builder {
    let mut b = hyper::server::conn::http1::Builder::new();
    b.timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(60))
        .max_buf_size(256 * 1024);
    b
}

fn http2() -> hyper::server::conn::http2::Builder<TokioExecutor> {
    let mut b = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
    b.timer(TokioTimer::new());
    b
}

pub async fn serve(daemon: Arc<Daemon>, session: Arc<Session>, stream: UnixStream) {
    // Pooled upstreams for the plain-HTTP requests on this guest connection.
    let pool = inspect::PlainPool::default();
    let svc = service_fn(move |req| {
        let (d, s, pool) = (daemon.clone(), session.clone(), pool.clone());
        async move { Ok::<_, Infallible>(handle(d, s, req, pool).await) }
    });
    let _ = http1()
        .serve_connection(TokioIo::new(stream), svc)
        .with_upgrades()
        .await;
}

async fn handle(
    d: Arc<Daemon>,
    s: Arc<Session>,
    req: Request<Incoming>,
    pool: inspect::PlainPool,
) -> Response<Body> {
    if req.method() == Method::CONNECT {
        connect(d, s, req).await
    } else {
        inspect::plain(d, s, req, pool).await
    }
}

/// Splits `host:port`, `[v6]:port` or `host`.
pub(crate) fn split_authority(a: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = a.strip_prefix('[') {
        let (h, tail) = rest.split_once(']')?;
        (h, tail.strip_prefix(':'))
    } else {
        match a.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, Some(p)),
            _ => (a, None),
        }
    };
    let port = match port {
        Some(p) => p.parse().ok()?,
        None => default_port,
    };
    // Untrusted input: only a valid host name or IP address passes.
    let host = zapper_policy::names::host(host).ok()?;
    Some((host, port))
}

struct Verdict {
    decision: Decision,
    effects: NetEffects,
    allowed: bool,
    /// Where an `ask` went, for a session whose asks a decider decides.
    pending: Option<Referral>,
}

impl Verdict {
    /// Why a request was refused, for the guest.
    fn denial(&self, what: &str) -> String {
        match &self.pending {
            Some(r) => format!(
                "access to {what} requested from {} (request {}); {}",
                r.to, r.id, r.then
            ),
            None => format!(
                "{what} denied by {}",
                self.decision.rule.as_deref().unwrap_or("default")
            ),
        }
    }
}

async fn decide(d: &Daemon, s: &Session, req: &NetRequest) -> Verdict {
    let (decision, effects) = s.with_layers(d, |layers| evaluate_net(layers, &s.caller_of(req)));
    let mut pending = None;
    let decider = d.deciders.first(s, |_| true);
    let allowed = match decision.action {
        Action::Allow => {
            if let (Some(dc), Some(rule)) = (&decider, &decision.rule) {
                dc.allowed(s, rule);
            }
            true
        }
        Action::Deny => false,
        Action::Ask => match &decider {
            // A decider's `ask` doesn't hold the request: it may take minutes.
            Some(dc) => {
                match dc.ask(s, req) {
                    Ok(r) => pending = Some(r),
                    Err(e) => tracing::warn!(session = %s.id, "referring an ask: {e:#}"),
                }
                false
            }
            // No decider: the operator answers; the suggested rule remembers a yes.
            None => {
                let subject = match (&req.method, &req.path) {
                    (Some(m), Some(p)) => format!("{m} https://{}:{}{p}", req.host, req.port),
                    _ => format!("connect {}:{}", req.host, req.port),
                };
                let suggestion = AnyRule::Net(NetRule {
                    id: None,
                    action: Action::Allow,
                    host: req.host.clone(),
                    ports: vec![req.port],
                    methods: vec![],
                    path: None,
                    protocol: Default::default(),
                    secrets: vec![],
                    set_headers: Default::default(),
                    final_: false,
                    comment: Some("approved".into()),
                });
                d.ask(s, Kind::Net, subject, suggestion).await
            }
        },
    };
    Verdict {
        decision,
        effects,
        allowed,
        pending,
    }
}

/// Copies a verdict into a request record.
fn note_verdict(rec: &httplog::Shared, v: &Verdict) {
    let mut r = rec.lock().unwrap();
    r.action = if v.allowed {
        Action::Allow
    } else {
        Action::Deny
    };
    r.rule = v.decision.rule.clone();
    r.scope = v.decision.scope.as_ref().map(|s| s.to_string());
}

/// Records a connection refused before anything was sent, so the request
/// log (and a firm's security officers) see refusals too.
fn note_refused(s: &Session, scheme: &'static str, req: &NetRequest, v: &Verdict) {
    let rec = s
        .requests
        .lock()
        .unwrap()
        .start_connect(scheme, &req.host, req.port);
    note_verdict(&rec, v);
    let why = v
        .pending
        .as_ref()
        .map(|r| format!("access requested from {} ({})", r.to, r.id));
    note_failure(s, &rec, 403, why);
}

/// Ends a record that got no upstream response. A 403 is a refusal, so
/// the record says it was denied, whatever said so.
fn note_failure(s: &Session, rec: &httplog::Shared, status: u16, error: Option<String>) {
    {
        let mut r = rec.lock().unwrap();
        r.status = Some(status);
        if status == 403 {
            r.action = Action::Deny;
        }
        r.error = error;
    }
    httplog::finish(&s.requests, rec);
}

/// Records the upstream response and wraps both bodies to capture them.
fn note_response(
    s: &Session,
    rec: &httplog::Shared,
    resp: Response<Body>,
    used: Vec<String>,
    values: Vec<String>,
) -> Response<Body> {
    httplog::mark_ttfb(rec);
    // A secret the proxy added never comes back to the guest.
    let masked = sanitize::mask_values(&values);
    let (mut parts, body) = resp.into_parts();
    sanitize::mask_headers(&mut parts.headers, &masked);
    {
        let mut r = rec.lock().unwrap();
        r.status = Some(parts.status.as_u16());
        r.resp_headers = httplog::headers(&parts.headers);
        r.secrets = used;
        r.redact = values;
    }
    let (rec, log) = (rec.clone(), s.requests.clone());
    let body = sanitize::Masked::wrap(body, masked);
    Response::from_parts(
        parts,
        httplog::Tee::wrap(body, rec, log, httplog::Side::Response),
    )
}

struct NetLog<'a> {
    req: &'a NetRequest,
    verdict: Option<&'a Verdict>,
    inspected: bool,
    status: Option<u16>,
    secrets: Vec<String>,
    error: Option<String>,
}

fn log(d: &Daemon, s: &Session, l: NetLog<'_>) {
    let allowed = l.verdict.is_some_and(|v| v.allowed) && l.error.is_none();
    let counter = if allowed {
        &s.counters.net_allowed
    } else {
        &s.counters.net_denied
    };
    counter.fetch_add(1, Ordering::Relaxed);
    d.emit(
        Some(s),
        EventBody::Net {
            host: l.req.host.clone(),
            port: l.req.port,
            method: l.req.method.clone(),
            path: l.req.path.clone(),
            action: if allowed { Action::Allow } else { Action::Deny },
            rule: l.verdict.and_then(|v| v.decision.rule.clone()),
            scope: l
                .verdict
                .and_then(|v| v.decision.scope.as_ref().map(|s| s.to_string())),
            inspected: l.inspected,
            status: l.status,
            secrets: l.secrets,
            error: l.error,
        },
    );
}

/// Where a connection goes: an address on the network, or a service the
/// daemon connects itself, by the name asked for.
#[derive(Clone)]
enum Target {
    Addr(addr::Checked),
    Service(String, u16, services::Target),
}

/// The target for `host:port`: a service by name, anything else resolved on
/// the host and checked against the address policy.
async fn target(d: &Daemon, s: &Session, host: &str, port: u16) -> Result<Target, String> {
    match d.services.route(s, host, port) {
        Some(services::Route::Service(t)) => Ok(Target::Service(host.to_string(), port, t)),
        Some(services::Route::Unserved) => Err(format!("nothing serves {host}:{port}")),
        None => resolve(d, host, port).await.map(Target::Addr),
    }
}

async fn dial_target(
    d: &Daemon,
    s: &Arc<Session>,
    t: &Target,
) -> std::io::Result<Box<dyn services::Io>> {
    match t {
        Target::Addr(a) => Ok(Box::new(dial(*a).await?)),
        Target::Service(host, port, t) => services::connect(d, s, host, *port, t)
            .await
            .map_err(|e| std::io::Error::other(format!("{e:#}"))),
    }
}

/// Resolves on the host and returns the first address sessions may reach.
async fn resolve(d: &Daemon, host: &str, port: u16) -> Result<addr::Checked, String> {
    let addrs: Vec<SocketAddr> = match tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::lookup_host((host, port)),
    )
    .await
    {
        Ok(Ok(a)) => a.collect(),
        Ok(Err(e)) => return Err(format!("resolving {host}: {e}")),
        Err(_) => return Err(format!("resolving {host}: timed out")),
    };
    let policy = d.addrs.read().unwrap();
    let mut why = "no addresses";
    for a in &addrs {
        match policy.checked(*a) {
            Ok(checked) => return Ok(checked),
            Err(e) => why = e,
        }
    }
    Err(format!("{host} resolves to a forbidden address: {why}"))
}

/// Connects to an address the address policy allowed.
pub(crate) async fn dial(addr: addr::Checked) -> std::io::Result<TcpStream> {
    let addr = addr.addr();
    match tokio::time::timeout(Duration::from_secs(15), TcpStream::connect(addr)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connect timed out",
        )),
    }
}

/// Decides a passthrough connection to `host:port` unless it will be inspected
/// per request, logging and recording a refusal. `scheme` labels the request
/// log (`connect`/`tcp`). `Ok` is the endpoint, its resolved target, and the
/// verdict (`None` when the connection will be inspected); `Err` is the
/// already-logged denial or target error, for the caller to answer with.
async fn prepare(
    d: &Daemon,
    s: &Session,
    host: String,
    port: u16,
    scheme: &'static str,
) -> Result<(NetRequest, Target, Option<Verdict>), String> {
    let endpoint = NetRequest {
        host,
        port,
        method: None,
        path: None,
        ..Default::default()
    };
    // Inspected tunnels are decided per request; others here, once. A name
    // no request to could be allowed is refused here even so, before it is
    // looked up on the host, as the DNS resolver refuses it: else a rule
    // with a path, which makes every connection to its host inspected,
    // would let a guest have any name looked up.
    let (inspect, reachable) = s.with_layers(d, |layers| {
        (
            needs_inspection(layers, &endpoint.host, port),
            resolvable(layers, &endpoint.host),
        )
    });
    if inspect && !reachable && d.services.route(s, &endpoint.host, port).is_none() {
        let why = format!("refused {} (no rule allows this name)", endpoint.host);
        log(
            d,
            s,
            NetLog {
                req: &endpoint,
                verdict: None,
                inspected: true,
                status: None,
                secrets: vec![],
                error: Some(why.clone()),
            },
        );
        return Err(why);
    }
    let verdict = if inspect {
        None
    } else {
        Some(decide(d, s, &endpoint).await)
    };
    if let Some(v) = &verdict {
        if !v.allowed {
            log(
                d,
                s,
                NetLog {
                    req: &endpoint,
                    verdict: Some(v),
                    inspected: false,
                    status: None,
                    secrets: vec![],
                    error: None,
                },
            );
            note_refused(s, scheme, &endpoint, v);
            return Err(v.denial(&format!("{}:{}", endpoint.host, port)));
        }
    }
    let addr = match target(d, s, &endpoint.host, port).await {
        Ok(a) => a,
        Err(e) => {
            log(
                d,
                s,
                NetLog {
                    req: &endpoint,
                    verdict: verdict.as_ref(),
                    inspected: inspect,
                    status: None,
                    secrets: vec![],
                    error: Some(e.clone()),
                },
            );
            return Err(e);
        }
    };
    Ok((endpoint, addr, verdict))
}

async fn connect(d: Arc<Daemon>, s: Arc<Session>, req: Request<Incoming>) -> Response<Body> {
    let Some((host, port)) = req
        .uri()
        .authority()
        .and_then(|a| split_authority(a.as_str(), 443))
    else {
        return text(StatusCode::BAD_REQUEST, "bad CONNECT target");
    };
    let (endpoint, addr, verdict) = match prepare(&d, &s, host, port, "connect").await {
        Ok(ready) => ready,
        Err(denial) => return text(StatusCode::FORBIDDEN, denial),
    };
    // The tunnel outlives the connection it was asked on, whose permit goes
    // when the upgrade takes it over: it holds one of its own.
    let Ok(permit) = s.runtime.conns().try_acquire_owned() else {
        return text(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many connections from this session",
        );
    };
    tokio::spawn(async move {
        let Ok(up) = hyper::upgrade::on(req).await else {
            return;
        };
        stream(d, s, TokioIo::new(up), endpoint, addr, verdict).await;
        drop(permit);
    });
    Response::builder()
        .status(StatusCode::OK)
        .body(
            Full::new(Bytes::new())
                .map_err(|n: Infallible| match n {})
                .boxed(),
        )
        .unwrap()
}

/// A connection the guest made through its NIC, captured outside the VM.
/// `ip` is what it dialled: a synthetic address from the session's DNS stands
/// for the name it was given out for, anything else for itself.
pub async fn transparent(d: Arc<Daemon>, s: Arc<Session>, io: UnixStream, ip: IpAddr, port: u16) {
    let host = s
        .dns
        .names()
        .name(ip)
        .map(str::to_string)
        .unwrap_or_else(|| ip.to_string());
    let (endpoint, addr, verdict) = match prepare(&d, &s, host, port, "tcp").await {
        Ok(ready) => ready,
        Err(_) => return,
    };
    stream(d, s, io, endpoint, addr, verdict).await;
}

/// Serves an allowed or to-be-inspected stream to `endpoint`. `verdict` is
/// `None` when requests are decided one by one.
async fn stream<S>(
    d: Arc<Daemon>,
    s: Arc<Session>,
    mut io: S,
    endpoint: NetRequest,
    addr: Target,
    verdict: Option<Verdict>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Some(first) = peek(&mut io).await else {
        return;
    };
    let is_tls = tls::starts_handshake(&first);
    match verdict {
        Some(v) if v.effects.protocol.is_db() => {
            relay::relay_database(&d, &s, io, first, &endpoint, &addr, v).await
        }
        Some(v) => tunnel(&d, &s, io, first, &endpoint, &addr, v).await,
        None if is_tls => {
            inspect::mitm(
                d,
                s,
                Prefixed::new(first, io),
                endpoint.host,
                endpoint.port,
                addr,
            )
            .await
        }
        None => {
            inspect::plain_inspected(
                d,
                s,
                Prefixed::new(first, io),
                endpoint.host,
                endpoint.port,
                addr,
            )
            .await
        }
    }
}

/// Reads what the client sends first: a whole TLS record if it starts one,
/// whatever arrives otherwise. Protocols where the server speaks first get an
/// empty buffer after 300 ms.
async fn peek<S: AsyncRead + Unpin>(io: &mut S) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // TLS clients send their ClientHello at once; protocols where the server
    // speaks first (SSH, SMTP, databases) must not wait long for nothing.
    let read = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            let n = io.read(&mut chunk).await?;
            if n == 0 {
                return Ok::<_, std::io::Error>(());
            }
            buf.extend_from_slice(&chunk[..n]);
            match tls::tls_record_len(&buf) {
                Some(len) if buf.len() < len && buf.len() < 17 * 1024 => continue,
                _ if buf.len() < 5 && tls::starts_handshake(&buf) => continue,
                _ => return Ok(()),
            }
        }
    })
    .await;
    match read {
        Ok(Err(_)) => None,
        _ => Some(buf),
    }
}

/// Whether a passed-through stream's first bytes may go to `host`: not TLS,
/// TLS without a server name, or a ClientHello naming `host`. A ClientHello
/// that doesn't parse (malformed, cut short, or split across records, which
/// servers accept) is refused: its server name can't be checked.
pub(crate) fn server_name_matches(first: &[u8], host: &str) -> Result<(), String> {
    match tls::client_hello_sni(first) {
        Ok(None) => Ok(()),
        Ok(Some(sni)) => match zapper_policy::names::hostname(&sni) {
            Ok(n) if n == host => Ok(()),
            Ok(n) => Err(format!("TLS server name {n} does not match the checked host")),
            Err(_) => Err("TLS server name (invalid) does not match the checked host".into()),
        },
        Err(()) => Err("a TLS ClientHello that can't be read (malformed, cut short or split across records): its server name can't be checked".into()),
    }
}

/// Passes a stream through. A TLS ClientHello must name the checked host, so
/// an allowed host cannot front for another.
async fn tunnel<S: AsyncRead + AsyncWrite + Unpin>(
    d: &Daemon,
    s: &Arc<Session>,
    mut io: S,
    first: Vec<u8>,
    endpoint: &NetRequest,
    addr: &Target,
    v: Verdict,
) {
    let scheme = if tls::starts_handshake(&first) {
        "tls"
    } else {
        "tcp"
    };
    let rec = s
        .requests
        .lock()
        .unwrap()
        .start_connect(scheme, &endpoint.host, endpoint.port);
    note_verdict(&rec, &v);
    let logp = |error: Option<String>| {
        log(
            d,
            s,
            NetLog {
                req: endpoint,
                verdict: Some(&v),
                inspected: false,
                status: None,
                secrets: vec![],
                error,
            },
        );
    };
    if let Err(error) = server_name_matches(&first, &endpoint.host) {
        logp(Some(error.clone()));
        note_failure(s, &rec, 403, Some(error));
        return;
    }
    let mut upstream = match dial_target(d, s, addr).await {
        Ok(u) => u,
        Err(e) => {
            logp(Some(e.to_string()));
            note_failure(s, &rec, 502, Some(e.to_string()));
            return;
        }
    };
    logp(None);
    httplog::mark_ttfb(&rec);
    if upstream.write_all(&first).await.is_ok() {
        if let Ok((up, down)) = tokio::io::copy_bidirectional(&mut io, &mut upstream).await {
            let mut r = rec.lock().unwrap();
            r.req_body.total = first.len() as u64 + up;
            r.resp_body.total = down;
        }
    }
    httplog::finish(&s.requests, &rec);
}

/// A stream that first replays bytes already read from it.
struct Prefixed<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = buf.remaining().min(self.prefix.len() - self.pos);
            let start = self.pos;
            buf.put_slice(&self.prefix[start..start + n]);
            self.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod approval_tests;

#[cfg(test)]
mod h2_tests;

#[cfg(test)]
mod hardening_tests;

#[cfg(test)]
mod tests {
    use super::inspect::{clean_headers, inject};
    use super::{server_name_matches, split_authority};
    use crate::testing::TestCore;
    use zapper_policy::{placeholder, NetEffects};

    /// A passed-through ClientHello must name the checked host; one whose
    /// name can't be read is refused, a hello split across two records
    /// included (servers accept it, so it would front for another host).
    #[test]
    fn a_tls_hello_names_the_checked_host_or_is_refused() {
        let hello = |name: &str| {
            let mut c = rustls::ClientConnection::new(
                crate::daemon::tls::client_config(),
                name.to_string().try_into().unwrap(),
            )
            .unwrap();
            let mut out = Vec::new();
            c.write_tls(&mut out).unwrap();
            out
        };
        let ok = hello("api.example");
        assert_eq!(server_name_matches(&ok, "api.example"), Ok(()));
        let e = server_name_matches(&hello("other.example"), "api.example").unwrap_err();
        assert!(e.contains("other.example"), "{e}");
        assert_eq!(
            server_name_matches(b"SSH-2.0-OpenSSH", "api.example"),
            Ok(())
        );
        // Cut short.
        assert!(server_name_matches(&ok[..40], "api.example").is_err());
        // The same handshake in two records, naming another host.
        let other = hello("other.example");
        let body = &other[5..];
        let (a, b) = body.split_at(20);
        let mut split = Vec::new();
        for part in [a, b] {
            split.extend_from_slice(&other[..3]);
            split.extend_from_slice(&(part.len() as u16).to_be_bytes());
            split.extend_from_slice(part);
        }
        let e = server_name_matches(&split, "api.example").unwrap_err();
        assert!(e.contains("split across records"), "{e}");
    }

    /// What goes upstream names the host that was checked, whatever Host
    /// the client sent, and carries no hop-by-hop headers.
    #[test]
    fn the_host_header_is_the_checked_host() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("host", "evil.example".parse().unwrap());
        headers.insert("proxy-authorization", "x".parse().unwrap());
        headers.insert("connection", "keep-alive".parse().unwrap());
        clean_headers(&mut headers, "api.example", 443, 443);
        assert_eq!(headers["host"], "api.example");
        assert!(headers.get("proxy-authorization").is_none());
        assert!(headers.get("connection").is_none());
        assert_eq!(headers.get_all("host").iter().count(), 1);
        // Another port is named.
        clean_headers(&mut headers, "api.example", 8443, 443);
        assert_eq!(headers["host"], "api.example:8443");
    }

    /// A secret goes only to its own `hosts`, and over plain HTTP only with
    /// `plaintext: true`, whichever rule asked for it.
    #[test]
    fn a_secret_goes_only_where_it_may() {
        let t = TestCore::new(
            "secrets:\n  s: { value: the-value, hosts: [api.example] }\n  lab: { value: lab-value, hosts: [lab.example], plaintext: true }\n",
        );
        let d = t.core.daemon();
        let add = |name: &str, host: &str, plaintext: bool| {
            let mut headers = hyper::HeaderMap::new();
            headers.insert(
                "authorization",
                format!("Bearer {}", placeholder(name)).parse().unwrap(),
            );
            let effects = NetEffects {
                secrets: vec![name.to_string()],
                set_headers: vec![],
                ..Default::default()
            };
            inject(d, host, plaintext, &effects, &mut headers).map(|i| (i.names, headers))
        };
        let (names, headers) = add("s", "api.example", false).unwrap();
        assert_eq!(names, ["s"]);
        assert_eq!(headers["authorization"], "Bearer the-value");
        // A rule matching another host asks for it in vain.
        let e = add("s", "evil.example", false).unwrap_err();
        assert!(e.contains("may not be sent to evil.example"), "{e}");
        // Plain HTTP, to its own host: refused without plaintext.
        let e = add("s", "api.example", true).unwrap_err();
        assert!(e.contains("plain HTTP"), "{e}");
        let (_, headers) = add("lab", "lab.example", true).unwrap();
        assert_eq!(headers["authorization"], "Bearer lab-value");
        // set_headers is held to the same.
        let mut headers = hyper::HeaderMap::new();
        let effects = NetEffects {
            secrets: vec![],
            set_headers: vec![("x-key".into(), "{{secret:s}}".into())],
            ..Default::default()
        };
        let e = inject(d, "evil.example", false, &effects, &mut headers)
            .err()
            .unwrap();
        assert!(e.contains("may not be sent to evil.example"), "{e}");
        assert!(headers.get("x-key").is_none());
    }

    #[test]
    fn authorities() {
        assert_eq!(
            split_authority("Example.com:8443", 443),
            Some(("example.com".into(), 8443))
        );
        assert_eq!(
            split_authority("example.com", 443),
            Some(("example.com".into(), 443))
        );
        assert_eq!(split_authority("[::1]:22", 443), Some(("::1".into(), 22)));
        assert_eq!(split_authority(":80", 443), None);
        assert_eq!(split_authority("evil host:443", 443), None);
        assert_eq!(split_authority("a..b:443", 443), None);
        assert_eq!(
            split_authority("example.com.:443", 443),
            Some(("example.com".into(), 443))
        );
    }

    /// Upstream URIs: HTTP/1 keeps the origin-form path; HTTP/2 is absolute,
    /// and the authority is always the checked host — a path cannot smuggle
    /// one in, because the scheme and authority are prefixed before it.
    #[test]
    fn upstream_uri_shape() {
        use super::inspect::upstream_uri;
        assert_eq!(
            upstream_uri("api.example", 443, "/a/b?q=1", false).to_string(),
            "/a/b?q=1"
        );
        assert_eq!(
            upstream_uri("api.example", 443, "/a/b?q=1", true).to_string(),
            "https://api.example/a/b?q=1"
        );
        // A non-default port is named in the authority.
        assert_eq!(
            upstream_uri("api.example", 8443, "/x", true).to_string(),
            "https://api.example:8443/x"
        );
        // Even a path that looks like it carries an authority stays the path.
        let u = upstream_uri("api.example", 443, "//evil.example/x", true);
        assert_eq!(u.authority().unwrap().host(), "api.example");
        assert_eq!(u.path(), "//evil.example/x");
        // An IPv6 literal is bracketed in the authority.
        assert_eq!(
            upstream_uri("::1", 8443, "/x", true).to_string(),
            "https://[::1]:8443/x"
        );
    }
}
