//! The product port: the board's way to what the firm serves at
//! `http://apps.<domain>`. Workers reach the product by that name, and the
//! engineers' ingress rules usually match it, so the port asks the cluster's
//! ingress for `apps.<domain>` whatever address the browser used, and maps
//! the name back to that address in what comes back: redirects, cookies,
//! and absolute links in pages, styles and scripts. WebSockets pass through.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{
    HeaderValue, ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HOST, LOCATION, ORIGIN, REFERER,
    SET_COOKIE, UPGRADE,
};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use super::FirmRun;

type Body = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// The largest page, style or script whose links are rewritten.
const MAX_REWRITE: usize = 16 << 20;

/// Serves one connection from the board's browser.
pub async fn serve(run: Arc<FirmRun>, conn: tokio::net::TcpStream, port: u32) {
    let svc = hyper::service::service_fn(move |req| {
        let run = run.clone();
        async move { Ok::<_, Infallible>(handle(&run, req, port).await) }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(conn), svc)
        .with_upgrades()
        .await;
}

fn text(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full(Bytes::from(format!("{msg}\n"))))
        .unwrap()
}

fn full(b: Bytes) -> Body {
    Full::new(b).map_err(|n: Infallible| match n {}).boxed()
}

/// `://from` replaced with `://to` in a header value or a text body.
fn swap(s: &str, from: &str, to: &str) -> String {
    s.replace(&format!("://{from}"), &format!("://{to}"))
}

/// `apps.<domain>` back to the board's address: the port speaks plain HTTP,
/// so `https://apps.<domain>` becomes `http://` too.
fn back(s: &str, apps: &str, shown: &str) -> String {
    swap(
        &s.replace(&format!("https://{apps}"), &format!("http://{shown}")),
        apps,
        shown,
    )
}

/// Response headers that may name the product's address.
const NAMING: [hyper::header::HeaderName; 6] = [
    LOCATION,
    hyper::header::CONTENT_LOCATION,
    hyper::header::REFRESH,
    hyper::header::LINK,
    hyper::header::ACCESS_CONTROL_ALLOW_ORIGIN,
    hyper::header::CONTENT_SECURITY_POLICY,
];

/// Drops a cookie's `Domain=` attribute: set for `apps.<domain>`, the
/// browser wouldn't send it back to the board's address.
fn host_only_cookie(v: &str) -> String {
    v.split(';')
        .filter(|a| !a.trim().to_ascii_lowercase().starts_with("domain="))
        .collect::<Vec<_>>()
        .join(";")
}

/// A body read whole goes out with its length: not chunked, as the
/// product may have sent it (both at once is a response hyper refuses,
/// and the browser gets nothing).
fn sized(h: &mut hyper::HeaderMap, len: usize) {
    h.remove(hyper::header::TRANSFER_ENCODING);
    h.insert(CONTENT_LENGTH, HeaderValue::from(len));
}

/// Whether a response's links are worth rewriting: pages, styles, scripts.
fn textual(ct: &str) -> bool {
    let ct = ct.to_ascii_lowercase();
    [
        "text/html",
        "text/css",
        "javascript",
        "application/json",
        "+xml",
    ]
    .iter()
    .any(|t| ct.contains(t))
}

async fn handle(run: &FirmRun, mut req: Request<Incoming>, port: u32) -> Response<Body> {
    let apps = format!("apps.{}", run.domain);
    // The address the board's browser used, to map `apps.<domain>` back to.
    let shown = req
        .headers()
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    let Some(id) = run
        .facility
        .cluster
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.id.clone())
    else {
        return text(StatusCode::BAD_GATEWAY, "the firm's cluster isn't running");
    };
    let io = match run.client.connect_port(&id, port).await {
        Ok(io) => io,
        Err(e) => return text(StatusCode::BAD_GATEWAY, &format!("the cluster: {e:#}")),
    };
    let (mut send, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(io)).await {
        Ok(x) => x,
        Err(e) => return text(StatusCode::BAD_GATEWAY, &format!("the cluster: {e}")),
    };
    tokio::spawn(conn.with_upgrades());
    let upgrading = req.headers().contains_key(UPGRADE);
    let client_up = upgrading.then(|| hyper::upgrade::on(&mut req));
    let h = req.headers_mut();
    h.insert(HOST, HeaderValue::from_str(&apps).unwrap());
    // Plain bodies, so their links can be rewritten.
    h.remove(ACCEPT_ENCODING);
    if !shown.is_empty() {
        for name in [ORIGIN, REFERER] {
            if let Some(v) = h.get(&name).and_then(|v| v.to_str().ok()) {
                if let Ok(v) = HeaderValue::from_str(&swap(v, &shown, &apps)) {
                    h.insert(name, v);
                }
            }
        }
    }
    let mut resp = match send.send_request(req).await {
        Ok(r) => r,
        Err(e) => return text(StatusCode::BAD_GATEWAY, &format!("the cluster: {e}")),
    };
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(client_up) = client_up {
            let server_up = hyper::upgrade::on(&mut resp);
            tokio::spawn(async move {
                if let (Ok(a), Ok(b)) = (client_up.await, server_up.await) {
                    let _ =
                        tokio::io::copy_bidirectional(&mut TokioIo::new(a), &mut TokioIo::new(b))
                            .await;
                }
            });
        }
        let (parts, _) = resp.into_parts();
        return Response::from_parts(parts, full(Bytes::new()));
    }
    let (mut parts, body) = resp.into_parts();
    if shown.is_empty() {
        return Response::from_parts(parts, body.map_err(Into::into).boxed());
    }
    // Redirects and the like: back to the address the board used, not a
    // name only the firm resolves.
    for name in NAMING {
        if let Some(v) = parts.headers.get(&name).and_then(|v| v.to_str().ok()) {
            if let Ok(v) = HeaderValue::from_str(&back(v, &apps, &shown)) {
                parts.headers.insert(name, v);
            }
        }
    }
    let cookies: Vec<HeaderValue> = parts
        .headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| HeaderValue::from_str(&host_only_cookie(v)).ok())
        .collect();
    if !cookies.is_empty() {
        parts.headers.remove(SET_COOKIE);
        for c in cookies {
            parts.headers.append(SET_COOKIE, c);
        }
    }
    let ct = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let small = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<usize>().ok())
        .is_none_or(|n| n <= MAX_REWRITE);
    if !textual(ct) || !small {
        return Response::from_parts(parts, body.map_err(Into::into).boxed());
    }
    let bytes = match Limited::new(body, MAX_REWRITE).collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => {
            return text(
                StatusCode::BAD_GATEWAY,
                &format!("the product's response: {e}"),
            )
        }
    };
    let bytes = match std::str::from_utf8(&bytes) {
        Ok(s) if s.contains(&format!("://{apps}")) => Bytes::from(back(s, &apps, &shown)),
        _ => bytes,
    };
    sized(&mut parts.headers, bytes.len());
    Response::from_parts(parts, full(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_maps_back_to_the_boards_address() {
        assert_eq!(
            swap(
                "http://apps.acme.internal/login?x=1",
                "apps.acme.internal",
                "127.0.0.1:8080"
            ),
            "http://127.0.0.1:8080/login?x=1"
        );
        // Only whole origins, not a path that mentions the name.
        assert_eq!(
            swap("/see/apps.acme.internal", "apps.acme.internal", "h:1"),
            "/see/apps.acme.internal"
        );
        assert_eq!(
            back(
                "https://apps.acme.internal/a",
                "apps.acme.internal",
                "127.0.0.1:8080"
            ),
            "http://127.0.0.1:8080/a"
        );
        assert_eq!(
            host_only_cookie("sid=1; Path=/; Domain=apps.acme.internal; HttpOnly"),
            "sid=1; Path=/; HttpOnly"
        );
        assert!(textual("text/html; charset=utf-8") && textual("application/javascript"));
        // A chunked page read whole goes out with its length only.
        let mut h = hyper::HeaderMap::new();
        h.insert(
            hyper::header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        sized(&mut h, 42);
        assert!(h.get(hyper::header::TRANSFER_ENCODING).is_none());
        assert_eq!(h[CONTENT_LENGTH], "42");
        assert!(!textual("image/png"));
    }
}
