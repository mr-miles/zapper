//! The HTTP(S) inspection datapath: the `CONNECT`/plain-HTTP path the proxy
//! takes when a rule needs to see or change requests. It terminates the
//! guest's TLS with the per-session CA, decides and cleans each request,
//! injects host-held secrets, and forwards to the real upstream (h1 or h2 by
//! ALPN). A child of `proxy`, so it reaches the core's decision/logging/dial
//! helpers directly; `stream()` and `handle()` enter it via mitm/plain/
//! plain_inspected.

use super::*;

enum Sender {
    H1(hyper::client::conn::http1::SendRequest<Body>),
    H2(hyper::client::conn::http2::SendRequest<Body>),
}

type Upstream = Arc<tokio::sync::Mutex<Option<Sender>>>;

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

pub(super) fn inject(
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

pub(super) async fn plain(
    d: Arc<Daemon>,
    s: Arc<Session>,
    req: Request<Incoming>,
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
    forward_plain(d, s, req, host, port).await
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
