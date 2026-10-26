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
use tokio_rustls::{TlsAcceptor, TlsConnector};
use zapper_policy::{
    evaluate_net, needs_inspection, placeholder, render, Action, AnyRule, Decision, Kind,
    NetEffects, NetRequest, NetRule,
};

use super::deciders::Referral;
use super::session::Session;
use super::{httplog, services, spec, tls, Daemon, EventBody};

type Body = BoxBody<Bytes, hyper::Error>;

const HOP_HEADERS: [&str; 7] = [
    "connection",
    "proxy-connection",
    "proxy-authorization",
    "proxy-authenticate",
    "keep-alive",
    "upgrade",
    "te",
];

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
    let svc = service_fn(move |req| {
        let (d, s) = (daemon.clone(), session.clone());
        async move { Ok::<_, Infallible>(handle(d, s, req).await) }
    });
    let _ = http1()
        .serve_connection(TokioIo::new(stream), svc)
        .with_upgrades()
        .await;
}

async fn handle(d: Arc<Daemon>, s: Arc<Session>, req: Request<Incoming>) -> Response<Body> {
    if req.method() == Method::CONNECT {
        connect(d, s, req).await
    } else {
        plain(d, s, req).await
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
    let (decision, effects) = {
        let c = d.config.read().unwrap();
        let layers = s.layers(&c);
        evaluate_net(&spec::layers_of(&layers), req)
    };
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
        // A decider's `ask` doesn't hold the request: it may take minutes.
        Action::Ask if decider.is_some() => {
            match decider.unwrap().ask(s, req) {
                Ok(r) => pending = Some(r),
                Err(e) => tracing::warn!(session = %s.id, "referring an ask: {e:#}"),
            }
            false
        }
        Action::Ask => {
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
                secrets: vec![],
                set_headers: Default::default(),
                final_: false,
                comment: Some("approved".into()),
            });
            d.ask(s, Kind::Net, subject, suggestion).await
        }
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
    let rec =
        s.requests
            .lock()
            .unwrap()
            .start(scheme, "CONNECT", &req.host, req.port, None, vec![]);
    note_verdict(&rec, v);
    let why = v
        .pending
        .as_ref()
        .map(|r| format!("access requested from {} ({})", r.to, r.id));
    note_failure(s, &rec, 403, why);
}

/// Ends a record that got no upstream response.
fn note_failure(s: &Session, rec: &httplog::Shared, status: u16, error: Option<String>) {
    {
        let mut r = rec.lock().unwrap();
        r.status = Some(status);
        if status == 403 && error.is_none() {
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
    resp: Response<Incoming>,
    used: Vec<String>,
    values: Vec<String>,
) -> Response<Body> {
    httplog::mark_ttfb(rec);
    {
        let mut r = rec.lock().unwrap();
        r.status = Some(resp.status().as_u16());
        r.resp_headers = httplog::headers(resp.headers());
        r.secrets = used;
        r.redact = values;
    }
    let (rec, log) = (rec.clone(), s.requests.clone());
    resp.map(|b| httplog::Tee::wrap(b.boxed(), rec, log, httplog::Side::Response))
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
    Addr(SocketAddr),
    Service(String, services::Target),
}

/// The target for `host:port`: a service by name, anything else resolved on
/// the host and checked against the address policy.
async fn target(d: &Daemon, s: &Session, host: &str, port: u16) -> Result<Target, String> {
    match d.services.route(s, host, port) {
        Some(services::Route::Service(t)) => Ok(Target::Service(host.to_string(), t)),
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
        Target::Service(host, t) => services::connect(d, s, host, t)
            .await
            .map_err(|e| std::io::Error::other(format!("{e:#}"))),
    }
}

/// Resolves on the host and returns the first address sessions may reach.
async fn resolve(d: &Daemon, host: &str, port: u16) -> Result<SocketAddr, String> {
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
        match policy.check(a.ip()) {
            Ok(()) => return Ok(*a),
            Err(e) => why = e,
        }
    }
    Err(format!("{host} resolves to a forbidden address: {why}"))
}

async fn dial(addr: SocketAddr) -> std::io::Result<TcpStream> {
    match tokio::time::timeout(Duration::from_secs(15), TcpStream::connect(addr)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connect timed out",
        )),
    }
}

async fn connect(d: Arc<Daemon>, s: Arc<Session>, req: Request<Incoming>) -> Response<Body> {
    let Some((host, port)) = req
        .uri()
        .authority()
        .and_then(|a| split_authority(a.as_str(), 443))
    else {
        return text(StatusCode::BAD_REQUEST, "bad CONNECT target");
    };
    let endpoint = NetRequest {
        host: host.clone(),
        port,
        method: None,
        path: None,
    };
    let inspect = {
        let c = d.config.read().unwrap();
        let layers = s.layers(&c);
        needs_inspection(&spec::layers_of(&layers), &host, port)
    };
    // Inspected tunnels are decided per request; others here, once.
    let verdict = if inspect {
        None
    } else {
        Some(decide(&d, &s, &endpoint).await)
    };
    if let Some(v) = &verdict {
        if !v.allowed {
            log(
                &d,
                &s,
                NetLog {
                    req: &endpoint,
                    verdict: Some(v),
                    inspected: false,
                    status: None,
                    secrets: vec![],
                    error: None,
                },
            );
            note_refused(&s, "connect", &endpoint, v);
            return text(StatusCode::FORBIDDEN, v.denial(&format!("{host}:{port}")));
        }
    }
    let addr = match target(&d, &s, &host, port).await {
        Ok(a) => a,
        Err(e) => {
            log(
                &d,
                &s,
                NetLog {
                    req: &endpoint,
                    verdict: verdict.as_ref(),
                    inspected: inspect,
                    status: None,
                    secrets: vec![],
                    error: Some(e.clone()),
                },
            );
            return text(StatusCode::FORBIDDEN, e);
        }
    };
    tokio::spawn(async move {
        let Ok(up) = hyper::upgrade::on(req).await else {
            return;
        };
        stream(d, s, TokioIo::new(up), endpoint, addr, verdict).await;
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
        .lock()
        .unwrap()
        .name(ip)
        .map(str::to_string)
        .unwrap_or_else(|| ip.to_string());
    let endpoint = NetRequest {
        host: host.clone(),
        port,
        method: None,
        path: None,
    };
    let inspect = {
        let c = d.config.read().unwrap();
        let layers = s.layers(&c);
        needs_inspection(&spec::layers_of(&layers), &host, port)
    };
    let verdict = if inspect {
        None
    } else {
        Some(decide(&d, &s, &endpoint).await)
    };
    if let Some(v) = &verdict {
        if !v.allowed {
            log(
                &d,
                &s,
                NetLog {
                    req: &endpoint,
                    verdict: Some(v),
                    inspected: false,
                    status: None,
                    secrets: vec![],
                    error: None,
                },
            );
            note_refused(&s, "tcp", &endpoint, v);
            return;
        }
    }
    let addr = match target(&d, &s, &host, port).await {
        Ok(a) => a,
        Err(e) => {
            log(
                &d,
                &s,
                NetLog {
                    req: &endpoint,
                    verdict: verdict.as_ref(),
                    inspected: inspect,
                    status: None,
                    secrets: vec![],
                    error: Some(e),
                },
            );
            return;
        }
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
    let is_tls = first.first() == Some(&0x16);
    match verdict {
        Some(v) => tunnel(&d, &s, io, first, &endpoint, &addr, v).await,
        None if is_tls => {
            mitm(
                d,
                s,
                Prefixed::new(first, io),
                endpoint.host,
                endpoint.port,
                addr,
            )
            .await
        }
        None => plain_inspected(d, s, Prefixed::new(first, io), endpoint.host, endpoint.port).await,
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
                _ if buf.len() < 5 && buf.first() == Some(&0x16) => continue,
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
    let scheme = if first.first() == Some(&0x16) {
        "tls"
    } else {
        "tcp"
    };
    let rec = s.requests.lock().unwrap().start(
        scheme,
        "CONNECT",
        &endpoint.host,
        endpoint.port,
        None,
        vec![],
    );
    note_verdict(&rec, &v);
    if let Err(error) = server_name_matches(&first, &endpoint.host) {
        log(
            d,
            s,
            NetLog {
                req: endpoint,
                verdict: Some(&v),
                inspected: false,
                status: None,
                secrets: vec![],
                error: Some(error.clone()),
            },
        );
        rec.lock().unwrap().action = Action::Deny;
        note_failure(s, &rec, 403, Some(error));
        return;
    }
    let mut upstream = match dial_target(d, s, addr).await {
        Ok(u) => u,
        Err(e) => {
            log(
                d,
                s,
                NetLog {
                    req: endpoint,
                    verdict: Some(&v),
                    inspected: false,
                    status: None,
                    secrets: vec![],
                    error: Some(e.to_string()),
                },
            );
            note_failure(s, &rec, 502, Some(e.to_string()));
            return;
        }
    };
    log(
        d,
        s,
        NetLog {
            req: endpoint,
            verdict: Some(&v),
            inspected: false,
            status: None,
            secrets: vec![],
            error: None,
        },
    );
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

/// The upstream side of an inspected connection: a reusable client speaking
/// whichever protocol the real server negotiated by ALPN.
enum Sender {
    H1(hyper::client::conn::http1::SendRequest<Body>),
    H2(hyper::client::conn::http2::SendRequest<Body>),
}

type Upstream = Arc<tokio::sync::Mutex<Option<Sender>>>;

async fn mitm<S>(d: Arc<Daemon>, s: Arc<Session>, io: S, host: String, port: u16, addr: Target)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let endpoint = NetRequest {
        host: host.clone(),
        port,
        method: None,
        path: None,
    };
    let config = match s.ca.server_config(&host) {
        Ok(c) => c,
        Err(e) => {
            log(
                &d,
                &s,
                NetLog {
                    req: &endpoint,
                    verdict: None,
                    inspected: true,
                    status: None,
                    secrets: vec![],
                    error: Some(format!("{e:#}")),
                },
            );
            return;
        }
    };
    let tls = match tokio::time::timeout(
        Duration::from_secs(15),
        TlsAcceptor::from(config).accept(io),
    )
    .await
    {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            log(
                &d,
                &s,
                NetLog {
                    req: &endpoint,
                    verdict: None,
                    inspected: true,
                    status: None,
                    secrets: vec![],
                    error: Some(format!("TLS from guest: {e}")),
                },
            );
            return;
        }
        Err(_) => return,
    };
    if let Some(sni) = tls.get_ref().1.server_name() {
        if !sni.eq_ignore_ascii_case(&host) {
            let error = Some(format!("TLS server name {sni} does not match CONNECT host"));
            log(
                &d,
                &s,
                NetLog {
                    req: &endpoint,
                    verdict: None,
                    inspected: true,
                    status: None,
                    secrets: vec![],
                    error,
                },
            );
            return;
        }
    }
    let h2 = tls.get_ref().1.alpn_protocol().is_some_and(|p| p == b"h2");
    let upstream: Upstream = Default::default();
    let svc = service_fn(move |req| {
        let (d, s, host, upstream, addr) = (
            d.clone(),
            s.clone(),
            host.clone(),
            upstream.clone(),
            addr.clone(),
        );
        async move { Ok::<_, Infallible>(inspected(d, s, req, host, port, addr, upstream).await) }
    });
    let io = TokioIo::new(tls);
    // Serve the guest in whichever protocol it negotiated by ALPN.
    if h2 {
        let _ = http2().serve_connection(io, svc).await;
    } else {
        let _ = http1().serve_connection(io, svc).await;
    }
}

/// Replaces secret placeholders in headers and applies `set_headers`, for
/// the secrets this host may receive. Returns the secrets used.
/// Secrets added to a request: their names, and their values, which the
/// request log replaces with `***` wherever they show up.
pub(crate) struct Injected {
    pub(crate) names: Vec<String>,
    pub(crate) values: Vec<String>,
}

fn inject(
    d: &Daemon,
    host: &str,
    plaintext: bool,
    effects: &NetEffects,
    headers: &mut hyper::HeaderMap,
) -> Result<Injected, String> {
    inject_with(&d.config.read().unwrap(), host, plaintext, effects, headers)
}

/// `inject` with the config given.
pub(crate) fn inject_with(
    config: &zapper_policy::Config,
    host: &str,
    plaintext: bool,
    effects: &NetEffects,
    headers: &mut hyper::HeaderMap,
) -> Result<Injected, String> {
    let mut used = Vec::new();
    let mut values = Vec::new();
    let value_of = |name: &str| -> Result<String, String> {
        let secret = config
            .secrets
            .get(name)
            .ok_or_else(|| format!("unknown secret {name}"))?;
        if !secret.allows_host(host) {
            return Err(format!("secret {name} may not be sent to {host}"));
        }
        if plaintext && !secret.plaintext {
            return Err(format!("secret {name} may not be sent over plain HTTP (set plaintext: true on it to allow)"));
        }
        secret
            .source
            .resolve()
            .map_err(|e| format!("secret {name}: {e:#}"))
    };
    for name in &effects.secrets {
        let ph = placeholder(name);
        let present = headers
            .values()
            .any(|v| v.to_str().is_ok_and(|s| s.contains(&ph)));
        if !present {
            continue;
        }
        let value = value_of(name)?;
        values.push(value.clone());
        for v in headers.values_mut() {
            if let Ok(s) = v.to_str() {
                if s.contains(&ph) {
                    *v = HeaderValue::from_str(&s.replace(&ph, &value))
                        .map_err(|_| format!("secret {name} is not a valid header value"))?;
                }
            }
        }
        used.push(name.clone());
    }
    for (k, template) in &effects.set_headers {
        let mut err = None;
        let value = render(template, |name| match value_of(name) {
            Ok(v) => {
                used.push(name.to_string());
                values.push(v.clone());
                v
            }
            Err(e) => {
                err = Some(e);
                String::new()
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        let name =
            HeaderName::from_bytes(k.as_bytes()).map_err(|_| format!("bad header name {k}"))?;
        headers.insert(
            name,
            HeaderValue::from_str(&value).map_err(|_| format!("bad value for header {k}"))?,
        );
    }
    used.sort();
    used.dedup();
    Ok(Injected {
        names: used,
        values,
    })
}

pub(crate) fn clean_headers(
    headers: &mut hyper::HeaderMap,
    host: &str,
    port: u16,
    default_port: u16,
) {
    for h in HOP_HEADERS {
        headers.remove(h);
    }
    // The Host header is the checked host, whatever the client sent.
    let host_header = if port == default_port {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    headers.insert(
        HOST,
        HeaderValue::from_str(&host_header).unwrap_or(HeaderValue::from_static("invalid")),
    );
}

/// The request URI to send upstream. HTTP/1 keeps the origin-form path the
/// guest sent; HTTP/2 needs an absolute URI so hyper can set `:authority` and
/// `:scheme`. `host` is the already-validated checked host and `path` the
/// already-parsed path-and-query, so neither can smuggle an authority.
pub(crate) fn upstream_uri(host: &str, port: u16, path: &str, h2: bool) -> hyper::Uri {
    if !h2 {
        return path.parse().unwrap_or_default();
    }
    // An IPv6 literal must be bracketed in an authority.
    let h = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let authority = if port == 443 {
        h
    } else {
        format!("{h}:{port}")
    };
    // Assemble through the builder so the authority is set independently of
    // the path: a path (even a malformed one) can never bleed into it.
    hyper::Uri::builder()
        .scheme("https")
        .authority(authority)
        .path_and_query(if path.is_empty() { "/" } else { path })
        .build()
        .unwrap_or_default()
}

async fn inspected(
    d: Arc<Daemon>,
    s: Arc<Session>,
    req: Request<Incoming>,
    host: String,
    port: u16,
    addr: Target,
    upstream: Upstream,
) -> Response<Body> {
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let net = NetRequest {
        host: host.clone(),
        port,
        method: Some(req.method().to_string()),
        path: Some(path.clone()),
    };
    // Headers as the guest sent them: before secrets are added.
    let rec = s.requests.lock().unwrap().start(
        "https",
        req.method().as_str(),
        &host,
        port,
        Some(&path),
        httplog::headers(req.headers()),
    );
    let v = decide(&d, &s, &net).await;
    note_verdict(&rec, &v);
    if !v.allowed {
        log(
            &d,
            &s,
            NetLog {
                req: &net,
                verdict: Some(&v),
                inspected: true,
                status: None,
                secrets: vec![],
                error: None,
            },
        );
        note_failure(&s, &rec, 403, None);
        return text(
            StatusCode::FORBIDDEN,
            v.denial(&format!(
                "{} {host}{path}",
                net.method.as_deref().unwrap_or("")
            )),
        );
    }
    let (mut parts, body) = req.into_parts();
    clean_headers(&mut parts.headers, &host, port, 443);
    let (used, values) = match inject(&d, &host, false, &v.effects, &mut parts.headers) {
        Ok(i) => (i.names, i.values),
        Err(e) => {
            log(
                &d,
                &s,
                NetLog {
                    req: &net,
                    verdict: Some(&v),
                    inspected: true,
                    status: None,
                    secrets: vec![],
                    error: Some(e.clone()),
                },
            );
            note_failure(&s, &rec, 403, Some(e.clone()));
            return text(StatusCode::FORBIDDEN, e);
        }
    };
    let mut guard = upstream.lock().await;
    let ready = match guard.as_mut() {
        Some(Sender::H1(sender)) => sender.ready().await.is_ok(),
        Some(Sender::H2(sender)) => sender.ready().await.is_ok(),
        None => false,
    };
    if !ready {
        match open_tls(&host, &addr).await {
            Ok(sender) => *guard = Some(sender),
            Err(e) => {
                log(
                    &d,
                    &s,
                    NetLog {
                        req: &net,
                        verdict: Some(&v),
                        inspected: true,
                        status: None,
                        secrets: used,
                        error: Some(e.clone()),
                    },
                );
                note_failure(&s, &rec, 502, Some(e.clone()));
                return text(StatusCode::BAD_GATEWAY, e);
            }
        }
    }
    // The upstream's negotiated protocol decides how we shape the request:
    // HTTP/2 needs an absolute URI (for `:authority`/`:scheme`); HTTP/1 keeps
    // the origin-form path. The Host header set by `clean_headers` stays and
    // agrees with the authority.
    let is_h2 = matches!(guard.as_ref(), Some(Sender::H2(_)));
    parts.uri = upstream_uri(&host, port, &path, is_h2);
    parts.version = if is_h2 {
        hyper::Version::HTTP_2
    } else {
        hyper::Version::HTTP_11
    };
    let body = httplog::Tee::wrap(
        body.boxed(),
        rec.clone(),
        s.requests.clone(),
        httplog::Side::Request,
    );
    let out = Request::from_parts(parts, body);
    // HTTP/2 multiplexes: clone the sender and release the lock so concurrent
    // streams on this guest connection run in parallel. HTTP/1 is serial, so
    // we hold the one sender across the send.
    let result = if is_h2 {
        let mut sender = match guard.as_mut() {
            Some(Sender::H2(s)) => s.clone(),
            _ => unreachable!(),
        };
        drop(guard);
        sender.send_request(out).await
    } else {
        let r = match guard.as_mut() {
            Some(Sender::H1(s)) => s.send_request(out).await,
            _ => unreachable!(),
        };
        drop(guard);
        r
    };
    match result {
        Ok(resp) => {
            log(
                &d,
                &s,
                NetLog {
                    req: &net,
                    verdict: Some(&v),
                    inspected: true,
                    status: Some(resp.status().as_u16()),
                    secrets: used.clone(),
                    error: None,
                },
            );
            note_response(&s, &rec, resp, used, values)
        }
        Err(e) => {
            log(
                &d,
                &s,
                NetLog {
                    req: &net,
                    verdict: Some(&v),
                    inspected: true,
                    status: None,
                    secrets: used,
                    error: Some(e.to_string()),
                },
            );
            note_failure(&s, &rec, 502, Some(e.to_string()));
            text(StatusCode::BAD_GATEWAY, e.to_string())
        }
    }
}

async fn open_tls(host: &str, addr: &Target) -> Result<Sender, String> {
    let Target::Addr(addr) = addr else {
        return Err(format!(
            "{host} is a service; rules with methods, paths or secrets don't apply to it"
        ));
    };
    let tcp = dial(*addr)
        .await
        .map_err(|e| format!("connecting to {host}: {e}"))?;
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("{host}: {e}"))?;
    let tls = TlsConnector::from(tls::client_config())
        .connect(name, tcp)
        .await
        .map_err(|e| format!("TLS to {host}: {e}"))?;
    // Speak whatever the server negotiated by ALPN.
    let h2 = tls.get_ref().1.alpn_protocol().is_some_and(|p| p == b"h2");
    if h2 {
        let (sender, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
                .await
                .map_err(|e| e.to_string())?;
        tokio::spawn(conn);
        Ok(Sender::H2(sender))
    } else {
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(conn);
        Ok(Sender::H1(sender))
    }
}

async fn plain(d: Arc<Daemon>, s: Arc<Session>, req: Request<Incoming>) -> Response<Body> {
    let uri = req.uri().clone();
    if uri.scheme_str() != Some("http") {
        return text(
            StatusCode::BAD_REQUEST,
            "expected an absolute http:// URI or CONNECT",
        );
    }
    let Some((host, port)) = uri
        .authority()
        .and_then(|a| split_authority(a.as_str(), 80))
    else {
        return text(StatusCode::BAD_REQUEST, "bad request target");
    };
    forward_plain(d, s, req, host, port).await
}

/// Inspects plain HTTP on a captured connection to `host:port`.
async fn plain_inspected<S>(d: Arc<Daemon>, s: Arc<Session>, io: S, host: String, port: u16)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let svc = service_fn(move |req| {
        let (d, s, host) = (d.clone(), s.clone(), host.clone());
        async move { Ok::<_, Infallible>(forward_plain(d, s, req, host, port).await) }
    });
    let _ = http1().serve_connection(TokioIo::new(io), svc).await;
}

/// Decides and forwards one plain HTTP request to `host:port`.
async fn forward_plain(
    d: Arc<Daemon>,
    s: Arc<Session>,
    req: Request<Incoming>,
    host: String,
    port: u16,
) -> Response<Body> {
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let net = NetRequest {
        host: host.clone(),
        port,
        method: Some(req.method().to_string()),
        path: Some(path.clone()),
    };
    let rec = s.requests.lock().unwrap().start(
        "http",
        req.method().as_str(),
        &host,
        port,
        Some(&path),
        httplog::headers(req.headers()),
    );
    let v = decide(&d, &s, &net).await;
    note_verdict(&rec, &v);
    if !v.allowed {
        log(
            &d,
            &s,
            NetLog {
                req: &net,
                verdict: Some(&v),
                inspected: true,
                status: None,
                secrets: vec![],
                error: None,
            },
        );
        note_failure(&s, &rec, 403, None);
        return text(
            StatusCode::FORBIDDEN,
            v.denial(&format!("{} http://{host}{path}", req.method())),
        );
    }
    let addr = match target(&d, &s, &host, port).await {
        Ok(a) => a,
        Err(e) => {
            log(
                &d,
                &s,
                NetLog {
                    req: &net,
                    verdict: Some(&v),
                    inspected: true,
                    status: None,
                    secrets: vec![],
                    error: Some(e.clone()),
                },
            );
            note_failure(&s, &rec, 403, Some(e.clone()));
            return text(StatusCode::FORBIDDEN, e);
        }
    };
    let (mut parts, body) = req.into_parts();
    clean_headers(&mut parts.headers, &host, port, 80);
    let (used, values) = match inject(&d, &host, true, &v.effects, &mut parts.headers) {
        Ok(i) => (i.names, i.values),
        Err(e) => {
            log(
                &d,
                &s,
                NetLog {
                    req: &net,
                    verdict: Some(&v),
                    inspected: true,
                    status: None,
                    secrets: vec![],
                    error: Some(e.clone()),
                },
            );
            note_failure(&s, &rec, 403, Some(e.clone()));
            return text(StatusCode::FORBIDDEN, e);
        }
    };
    parts.uri = path.parse().unwrap_or_default();
    let body = httplog::Tee::wrap(
        body.boxed(),
        rec.clone(),
        s.requests.clone(),
        httplog::Side::Request,
    );
    let out = Request::from_parts(parts, body);
    let result = async {
        let tcp = dial_target(&d, &s, &addr)
            .await
            .map_err(|e| e.to_string())?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake::<_, Body>(TokioIo::new(tcp))
                .await
                .map_err(|e| e.to_string())?;
        tokio::spawn(conn);
        sender.send_request(out).await.map_err(|e| e.to_string())
    }
    .await;
    match result {
        Ok(resp) => {
            log(
                &d,
                &s,
                NetLog {
                    req: &net,
                    verdict: Some(&v),
                    inspected: true,
                    status: Some(resp.status().as_u16()),
                    secrets: used.clone(),
                    error: None,
                },
            );
            note_response(&s, &rec, resp, used, values)
        }
        Err(e) => {
            log(
                &d,
                &s,
                NetLog {
                    req: &net,
                    verdict: Some(&v),
                    inspected: true,
                    status: None,
                    secrets: used,
                    error: Some(e.clone()),
                },
            );
            note_failure(&s, &rec, 502, Some(e.clone()));
            text(StatusCode::BAD_GATEWAY, e)
        }
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
    use super::{clean_headers, inject, server_name_matches, split_authority};
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
        use super::upstream_uri;
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
