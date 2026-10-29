//! A small HTTP/1.1 client for GitHub's API: https in production, plain http
//! for a fake GitHub in tests.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{header, Method, Request};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(30);
/// More than any response of the calls made here.
const MAX_BODY: usize = 4 << 20;

pub struct Response {
    pub status: u16,
    pub body: Bytes,
    /// A redirect's target.
    pub location: Option<String>,
}

/// One request; `bearer` is the JWT or token, `body` a JSON document.
pub async fn send(
    method: Method,
    url: &str,
    bearer: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Result<Response> {
    tokio::time::timeout(TIMEOUT, send_inner(method, url, bearer, body))
        .await
        .map_err(|_| anyhow!("timed out"))?
}

async fn send_inner(
    method: Method,
    url: &str,
    bearer: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Result<Response> {
    let (tls, host, port, path) = split(url)?;
    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    let mut req = Request::builder()
        .method(method)
        .uri(&path)
        .header(header::HOST, &host)
        .header(header::USER_AGENT, "zapper")
        .header(header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(b) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {b}"));
    }
    let payload = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Bytes::from(serde_json::to_vec(v)?)
        }
        None => Bytes::new(),
    };
    let req = req.body(Full::new(payload))?;
    if tls {
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| anyhow!("{host:?} is not a server name"))?;
        let stream = tokio_rustls::TlsConnector::from(client_config())
            .connect(name, tcp)
            .await
            .with_context(|| format!("TLS to {host}"))?;
        exchange(TokioIo::new(stream), req).await
    } else {
        exchange(TokioIo::new(tcp), req).await
    }
}

async fn exchange<T>(io: TokioIo<T>, req: Request<Full<Bytes>>) -> Result<Response>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
    tokio::spawn(conn);
    let resp = sender.send_request(req).await?;
    let status = resp.status().as_u16();
    let location = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut body = resp.into_body();
    let mut buf = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Some(data) = frame?.data_ref() {
            buf.extend_from_slice(data);
            if buf.len() > MAX_BODY {
                bail!("response too large");
            }
        }
    }
    Ok(Response {
        status,
        body: buf.into(),
        location,
    })
}

fn client_config() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// `(tls, host, port, path and query)`.
fn split(url: &str) -> Result<(bool, String, u16, String)> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        bail!("{url:?} is not an http(s) URL");
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| anyhow!("bad port in {url:?}"))?),
        None => (authority, if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        bail!("{url:?} has no host");
    }
    Ok((tls, host.into(), port, path.into()))
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn splits_urls() {
        assert_eq!(
            split("https://api.github.com/app").unwrap(),
            (true, "api.github.com".into(), 443, "/app".into())
        );
        assert_eq!(
            split("http://127.0.0.1:8080").unwrap(),
            (false, "127.0.0.1".into(), 8080, "/".into())
        );
        assert!(split("ftp://x").is_err());
        assert!(split("https:///x").is_err());
    }
}
