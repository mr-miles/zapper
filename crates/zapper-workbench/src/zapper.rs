//! zapper's API, on its unix socket: the workbench is its operator, and the
//! one client with clients of its own.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::UnixStream;

/// Where zapper listens: `$ZAPPER_SOCKET`, else under `$XDG_RUNTIME_DIR`, as
/// the `zapper` CLI finds it.
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("ZAPPER_SOCKET") {
        return p.into();
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/zapper-{}", unsafe { libc::getuid() })));
    base.join("zapper").join("api.sock")
}

/// A client of zapper's API.
#[derive(Debug, Clone)]
pub struct Zapper {
    socket: PathBuf,
}

/// What zapper answered: its status and its JSON (`null` for none).
pub struct Answer {
    pub status: StatusCode,
    pub body: Value,
}

impl Answer {
    /// The JSON, if zapper said yes; its error, with its status, if not.
    pub fn ok(self) -> std::result::Result<Value, (StatusCode, String)> {
        if self.status.is_success() {
            return Ok(self.body);
        }
        let why = self.body["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| self.body.to_string());
        Err((self.status, why))
    }
}

impl Zapper {
    pub fn new(socket: PathBuf) -> Self {
        Zapper { socket }
    }

    /// Sends `req`, on a connection of its own (which an upgrade keeps).
    async fn send(&self, req: Request<Full<Bytes>>) -> Result<Response<Incoming>> {
        let stream = UnixStream::connect(&self.socket).await.with_context(|| {
            format!(
                "zapper at {} (is `zapper serve` running?)",
                self.socket.display()
            )
        })?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(conn.with_upgrades());
        Ok(sender.send_request(req).await?)
    }

    fn request(
        method: Method,
        path: &str,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<Request<Full<Bytes>>> {
        let mut b = Request::builder()
            .method(method)
            .uri(format!("http://zapper{path}"))
            .header("host", "zapper");
        for (k, v) in headers {
            b = b.header(k, v);
        }
        Ok(b.body(Full::new(body))?)
    }

    /// `method path` with a JSON `body`, and what zapper answered.
    pub async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Answer> {
        let mut headers = HeaderMap::new();
        let body = match body {
            Some(b) => {
                headers.insert("content-type", "application/json".parse()?);
                Bytes::from(serde_json::to_vec(b)?)
            }
            None => Bytes::new(),
        };
        let resp = self
            .send(Self::request(method, path, &headers, body)?)
            .await?;
        let status = resp.status();
        let bytes = resp.into_body().collect().await?.to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        Ok(Answer { status, body })
    }

    /// `GET path` as it comes: for a body that streams (server-sent events,
    /// an audit log), passed on as zapper sends it.
    pub async fn stream(&self, path: &str, headers: &HeaderMap) -> Result<Response<Incoming>> {
        self.send(Self::request(Method::GET, path, headers, Bytes::new())?)
            .await
    }

    /// `GET path` upgraded (a terminal): the stream, and the terminal's
    /// number if zapper gave one.
    pub async fn upgrade(
        &self,
        path: &str,
    ) -> Result<(TokioIo<hyper::upgrade::Upgraded>, Option<String>)> {
        let mut headers = HeaderMap::new();
        headers.insert("connection", "upgrade".parse()?);
        headers.insert("upgrade", "zapper-shell".parse()?);
        let resp = self
            .send(Self::request(Method::GET, path, &headers, Bytes::new())?)
            .await?;
        if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
            let status = resp.status();
            let bytes = resp.into_body().collect().await?.to_bytes();
            let why = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
            bail!("{status}: {why}");
        }
        let n = resp
            .headers()
            .get("x-zapper-terminal")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok((TokioIo::new(hyper::upgrade::on(resp).await?), n))
    }
}
