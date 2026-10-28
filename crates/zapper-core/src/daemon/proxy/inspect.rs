//! The HTTP(S) inspection datapath: the `CONNECT`/plain-HTTP path the proxy
//! takes when a rule needs to see or change requests. It terminates the
//! guest's TLS with the per-session CA, decides and cleans each request,
//! injects host-held secrets, and forwards to the real upstream (h1 or h2 by
//! ALPN). A child of `proxy`, so it reaches the core's decision/logging/dial
//! helpers directly; `stream()` and `handle()` enter it via mitm/plain/
//! plain_inspected.

use std::collections::{BTreeMap, HashMap};

use super::*;

enum Sender {
    H1(hyper::client::conn::http1::SendRequest<Body>),
    H2(hyper::client::conn::http2::SendRequest<Body>),
}

/// The pooled upstream for one inspected (TLS-terminated) guest connection: at
/// most one live `Sender`, reconnected when missing or stale, shared across the
/// connection's requests.
#[derive(Clone, Default)]
struct Upstream(Arc<tokio::sync::Mutex<Option<Sender>>>);

impl Upstream {
    /// Sends one request, reusing the pooled connection (reconnecting via
    /// `open_tls` when missing or not ready), then calls `shape` with the
    /// negotiated protocol (`true` = HTTP/2, which needs an absolute URI) to
    /// build the request and dispatches it. HTTP/2 multiplexes — clone the
    /// sender and release the lock so concurrent streams run in parallel;
    /// HTTP/1 is serial, so hold the sender across the send.
    async fn send(
        &self,
        host: &str,
        addr: &Target,
        shape: impl FnOnce(bool) -> Request<Body>,
    ) -> Result<Response<Incoming>, String> {
        let mut guard = self.0.lock().await;
        let ready = match guard.as_mut() {
            Some(Sender::H1(sender)) => sender.ready().await.is_ok(),
            Some(Sender::H2(sender)) => sender.ready().await.is_ok(),
            None => false,
        };
        if !ready {
            *guard = Some(open_tls(host, addr).await?);
        }
        let is_h2 = matches!(guard.as_ref(), Some(Sender::H2(_)));
        let out = shape(is_h2);
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
        result.map_err(|e| e.to_string())
    }
}

type SendRequestH1 = hyper::client::conn::http1::SendRequest<Body>;

/// Pooled plain-HTTP/1 upstreams for one guest connection, keyed by origin, so
/// the connection's requests reuse upstream connections instead of opening one
/// per request (which a connection-rate firewall may ban). The TLS path pools
/// via `Upstream`; this is the `http://` and captured-plain-HTTP path, keyed by
/// origin because one proxy connection can carry several.
#[derive(Clone, Default)]
pub(super) struct PlainPool(Arc<tokio::sync::Mutex<HashMap<(String, u16), SendRequestH1>>>);

impl PlainPool {
    /// Sends `out` to `target` for origin `host:port`, reusing a live pooled
    /// HTTP/1 connection when there is one, else dialing a fresh one. A service
    /// (in-process duplex, not a real upstream to rate-limit) is never pooled.
    async fn send(
        &self,
        d: &Daemon,
        s: &Arc<Session>,
        host: &str,
        port: u16,
        target: &Target,
        out: Request<Body>,
    ) -> Result<Response<Incoming>, String> {
        if !matches!(target, Target::Addr(_)) {
            return dial_plain(d, s, target)
                .await?
                .send_request(out)
                .await
                .map_err(|e| e.to_string());
        }
        let key = (host.to_string(), port);
        let mut pool = self.0.lock().await;
        let ready = match pool.get_mut(&key) {
            Some(sender) => sender.ready().await.is_ok(),
            None => false,
        };
        if !ready {
            pool.insert(key.clone(), dial_plain(d, s, target).await?);
        }
        // The guest connection is served serially, so at most one request per
        // origin is in flight; holding the lock across the send keeps reuse
        // correct (the next request waits for the sender to be ready again
        // rather than opening another connection).
        let r = pool.get_mut(&key).unwrap().send_request(out).await;
        drop(pool);
        r.map_err(|e| e.to_string())
    }
}

/// Dials a fresh plain-HTTP/1 upstream to `target`, spawning its connection.
async fn dial_plain(
    d: &Daemon,
    s: &Arc<Session>,
    target: &Target,
) -> Result<SendRequestH1, String> {
    let tcp = dial_target(d, s, target).await.map_err(|e| e.to_string())?;
    let (sender, conn) = hyper::client::conn::http1::handshake::<_, Body>(TokioIo::new(tcp))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(conn);
    Ok(sender)
}

/// Logs a policy refusal of an inspected request, finishes its record, and
/// returns the 403 for the guest.
fn deny(
    d: &Daemon,
    s: &Session,
    rec: &httplog::Shared,
    net: &NetRequest,
    v: &Verdict,
    denial: String,
) -> Response<Body> {
    log(
        d,
        s,
        NetLog {
            req: net,
            verdict: Some(v),
            inspected: true,
            status: None,
            secrets: vec![],
            error: None,
        },
    );
    note_failure(s, rec, 403, None);
    text(StatusCode::FORBIDDEN, denial)
}

/// Logs an inspected request that failed to forward, finishes its record, and
/// returns the error response. `status` is 403 (bad target or secret) or 502
/// (upstream); `secrets` is whatever had already been injected.
#[allow(clippy::too_many_arguments)]
fn fail(
    d: &Daemon,
    s: &Session,
    rec: &httplog::Shared,
    net: &NetRequest,
    v: &Verdict,
    status: StatusCode,
    secrets: Vec<String>,
    error: String,
) -> Response<Body> {
    log(
        d,
        s,
        NetLog {
            req: net,
            verdict: Some(v),
            inspected: true,
            status: None,
            secrets,
            error: Some(error.clone()),
        },
    );
    note_failure(s, rec, status.as_u16(), Some(error.clone()));
    text(status, error)
}

/// Logs an inspected request's upstream response and returns it to the guest.
#[allow(clippy::too_many_arguments)]
fn responded(
    d: &Daemon,
    s: &Session,
    rec: &httplog::Shared,
    net: &NetRequest,
    v: &Verdict,
    resp: Response<Incoming>,
    used: Vec<String>,
    values: Vec<String>,
) -> Response<Body> {
    log(
        d,
        s,
        NetLog {
            req: net,
            verdict: Some(v),
            inspected: true,
            status: Some(resp.status().as_u16()),
            secrets: used.clone(),
            error: None,
        },
    );
    note_response(s, rec, resp, used, values)
}

pub(super) async fn mitm<S>(
    d: Arc<Daemon>,
    s: Arc<Session>,
    io: S,
    host: String,
    port: u16,
    addr: Target,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let endpoint = NetRequest {
        host: host.clone(),
        port,
        method: None,
        path: None,
    };
    let config = match s.tls.server_config(&host) {
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

#[cfg(test)]
pub(super) fn inject(
    d: &Daemon,
    host: &str,
    plaintext: bool,
    effects: &NetEffects,
    headers: &mut hyper::HeaderMap,
) -> Result<Injected, String> {
    inject_with(
        &d.config.read().unwrap(),
        &BTreeMap::new(),
        host,
        plaintext,
        effects,
        headers,
        &HashMap::new(),
    )
}

/// `inject` for a session's request: the config's secrets, then the
/// session's own. Command secrets it will use are minted first, without
/// holding a thread while they are.
pub(super) async fn inject_for(
    d: &Daemon,
    s: &Session,
    host: &str,
    plaintext: bool,
    effects: &NetEffects,
    headers: &mut hyper::HeaderMap,
) -> Result<Injected, String> {
    let own = s.secrets.read().unwrap().clone();
    let to_mint: Vec<(String, zapper_policy::mint::Spec)> = {
        let config = d.config.read().unwrap();
        used(effects, headers)
            .into_iter()
            .filter_map(|name| {
                let secret = config.secrets.get(&name).or_else(|| own.get(&name))?;
                let sendable = secret.allows_host(host) && (!plaintext || secret.plaintext);
                let aws = secret.mode == zapper_policy::SecretMode::Aws;
                let spec = secret.source.mint_spec().filter(|_| sendable && !aws)?;
                Some((name, spec))
            })
            .collect()
    };
    let mut minted = HashMap::new();
    for (name, spec) in to_mint {
        let v = crate::mint::resolve(&spec)
            .await
            .map_err(|e| format!("{e:#}"));
        minted.insert(name, v);
    }
    inject_with(
        &d.config.read().unwrap(),
        &own,
        host,
        plaintext,
        effects,
        headers,
        &minted,
    )
}

/// The secrets a request would use: those of the rule's whose placeholder
/// it carries, and those its headers are set from.
fn used(effects: &NetEffects, headers: &hyper::HeaderMap) -> Vec<String> {
    let mut names: Vec<String> = effects
        .secrets
        .iter()
        .filter(|name| {
            let ph = placeholder(name);
            headers
                .values()
                .any(|v| v.to_str().is_ok_and(|s| s.contains(&ph)))
        })
        .cloned()
        .collect();
    for (_, template) in &effects.set_headers {
        names.extend(zapper_policy::secret_refs(template));
    }
    names.sort();
    names.dedup();
    names
}

/// `inject` with the config and the session's secrets given, and the values
/// of the command secrets already minted (by name).
pub(crate) fn inject_with(
    config: &zapper_policy::Config,
    own: &BTreeMap<String, zapper_policy::Secret>,
    host: &str,
    plaintext: bool,
    effects: &NetEffects,
    headers: &mut hyper::HeaderMap,
    minted: &HashMap<String, Result<String, String>>,
) -> Result<Injected, String> {
    let mut used = Vec::new();
    let mut values = Vec::new();
    let value_of = |name: &str| -> Result<String, String> {
        let secret = config
            .secrets
            .get(name)
            .or_else(|| own.get(name))
            .ok_or_else(|| format!("unknown secret {name}"))?;
        if !secret.allows_host(host) {
            return Err(format!("secret {name} may not be sent to {host}"));
        }
        if plaintext && !secret.plaintext {
            return Err(format!("secret {name} may not be sent over plain HTTP (set plaintext: true on it to allow)"));
        }
        match secret.source.mint_spec() {
            Some(_) => match minted.get(name) {
                Some(v) => v
                    .clone()
                    .map_err(|e| format!("secret {name}: minting the secret: {e}")),
                None => Err(format!("secret {name}: not minted")),
            },
            None => secret
                .source
                .resolve()
                .map_err(|e| format!("secret {name}: {e:#}")),
        }
    };
    for name in &effects.secrets {
        // An `aws` secret signs the request (`aws::resign`); its value never
        // goes into a header.
        let aws = config.secrets.get(name).or_else(|| own.get(name));
        if aws.is_some_and(|s| s.mode == zapper_policy::SecretMode::Aws) {
            continue;
        }
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

/// Decides, cleans, injects, and forwards one inspected request, then logs the
/// outcome. The shared body of the TLS (`inspected`) and plain (`forward_plain`)
/// paths; `send` shapes the request for the negotiated protocol and dispatches
/// it through the path's pooled upstream, reporting its own 403/502.
#[allow(clippy::too_many_arguments)]
async fn forward<F, Fut>(
    d: &Arc<Daemon>,
    s: &Arc<Session>,
    req: Request<Incoming>,
    host: String,
    port: u16,
    scheme: &'static str,
    default_port: u16,
    plaintext: bool,
    send: F,
) -> Response<Body>
where
    F: FnOnce(hyper::http::request::Parts, Body, String) -> Fut,
    Fut: std::future::Future<Output = Result<Response<Incoming>, (StatusCode, String)>>,
{
    let sent = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    // Rules judge the path the upstream will act on, and that is the path
    // forwarded; one whose meaning depends on the upstream is refused.
    let normalized = super::sanitize::normalize_path(&sent);
    let path = normalized.clone().unwrap_or_else(|_| sent.clone());
    let net = NetRequest {
        host: host.clone(),
        port,
        method: Some(req.method().to_string()),
        path: Some(path.clone()),
    };
    // Headers as the guest sent them: before secrets are added.
    let rec = s.requests.lock().unwrap().start(
        scheme,
        req.method().as_str(),
        &host,
        port,
        Some(&path),
        httplog::headers(req.headers()),
    );
    let refused = match normalized {
        Err(why) => Some((StatusCode::BAD_REQUEST, format!("{why}: {sent}"))),
        Ok(_) => super::sanitize::method_override(req.method().as_str(), &path, req.headers())
            .map(|why| (StatusCode::FORBIDDEN, why)),
    };
    if let Some((status, why)) = refused {
        log(
            d,
            s,
            NetLog {
                req: &net,
                verdict: None,
                inspected: true,
                status: None,
                secrets: vec![],
                error: Some(why.clone()),
            },
        );
        note_failure(s, &rec, status.as_u16(), Some(why.clone()));
        return text(status, why);
    }
    let v = decide(d, s, &net).await;
    note_verdict(&rec, &v);
    if !v.allowed {
        let denial = v.denial(&format!(
            "{} {scheme}://{host}{path}",
            net.method.as_deref().unwrap_or("")
        ));
        return deny(d, s, &rec, &net, &v, denial);
    }
    let (mut parts, body) = req.into_parts();
    clean_headers(&mut parts.headers, &host, port, default_port);
    let (mut used, mut values) =
        match inject_for(d, s, &host, plaintext, &v.effects, &mut parts.headers).await {
            Ok(i) => (i.names, i.values),
            Err(e) => return fail(d, s, &rec, &net, &v, StatusCode::FORBIDDEN, vec![], e),
        };
    // A response carrying a secret's value back must be readable to be
    // masked: ask for it uncompressed.
    if !values.is_empty() {
        parts.headers.insert(
            hyper::header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
    }
    // Signed with an `aws` secret's fake key: signed again with the real one.
    let (body, path) = match super::aws::resign(
        d,
        s,
        &host,
        plaintext,
        &v.effects.secrets,
        &mut parts,
        body.boxed(),
        &path,
    )
    .await
    {
        Ok(super::aws::Outcome::Resigned(r)) => {
            used.push(r.secret);
            values.extend(r.mask);
            (r.body, r.path)
        }
        Ok(super::aws::Outcome::Unsigned(body)) => (body, path),
        Err(e) => return fail(d, s, &rec, &net, &v, StatusCode::FORBIDDEN, used, e),
    };
    let body = httplog::Tee::wrap(
        body,
        rec.clone(),
        s.requests.clone(),
        httplog::Side::Request,
    );
    match send(parts, body, path).await {
        Ok(resp) => responded(d, s, &rec, &net, &v, resp, used, values),
        Err((status, e)) => fail(d, s, &rec, &net, &v, status, used, e),
    }
}

/// An inspected request over a TLS-terminated connection, forwarded through the
/// per-connection `Upstream` pool (h1 or h2 by the upstream's ALPN).
async fn inspected(
    d: Arc<Daemon>,
    s: Arc<Session>,
    req: Request<Incoming>,
    host: String,
    port: u16,
    addr: Target,
    upstream: Upstream,
) -> Response<Body> {
    forward(
        &d,
        &s,
        req,
        host.clone(),
        port,
        "https",
        443,
        false,
        move |parts, body, path| async move {
            // Shape the request for the negotiated protocol: HTTP/2 needs an
            // absolute URI (for `:authority`/`:scheme`); HTTP/1 keeps the
            // origin-form path. The Host header from `clean_headers` agrees.
            let uri_host = host.clone();
            upstream
                .send(&host, &addr, move |is_h2| {
                    let mut parts = parts;
                    parts.uri = upstream_uri(&uri_host, port, &path, is_h2);
                    parts.version = if is_h2 {
                        hyper::Version::HTTP_2
                    } else {
                        hyper::Version::HTTP_11
                    };
                    Request::from_parts(parts, body)
                })
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e))
        },
    )
    .await
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

pub(super) async fn plain(
    d: Arc<Daemon>,
    s: Arc<Session>,
    req: Request<Incoming>,
    pool: PlainPool,
) -> Response<Body> {
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
    forward_plain(d, s, req, host, port, pool).await
}

/// Inspects plain HTTP on a captured connection to `host:port`.
pub(super) async fn plain_inspected<S>(
    d: Arc<Daemon>,
    s: Arc<Session>,
    io: S,
    host: String,
    port: u16,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // One pool for this captured connection's requests (all to one origin).
    let pool = PlainPool::default();
    let svc = service_fn(move |req| {
        let (d, s, host, pool) = (d.clone(), s.clone(), host.clone(), pool.clone());
        async move { Ok::<_, Infallible>(forward_plain(d, s, req, host, port, pool).await) }
    });
    let _ = http1().serve_connection(TokioIo::new(io), svc).await;
}

/// An inspected plain-HTTP request to `host:port`, forwarded through the
/// per-connection, origin-keyed `PlainPool`. The target is resolved here (a
/// bad/forbidden one is a 403) since a plain proxy connection is multi-origin.
async fn forward_plain(
    d: Arc<Daemon>,
    s: Arc<Session>,
    req: Request<Incoming>,
    host: String,
    port: u16,
    pool: PlainPool,
) -> Response<Body> {
    let (dc, sc) = (d.clone(), s.clone());
    forward(
        &d,
        &s,
        req,
        host.clone(),
        port,
        "http",
        80,
        true,
        move |mut parts, body, path| async move {
            let addr = target(&dc, &sc, &host, port)
                .await
                .map_err(|e| (StatusCode::FORBIDDEN, e))?;
            parts.uri = path.parse().unwrap_or_default();
            pool.send(
                &dc,
                &sc,
                &host,
                port,
                &addr,
                Request::from_parts(parts, body),
            )
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))
        },
    )
    .await
}
