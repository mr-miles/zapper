//! Client of the daemon's unix-socket API.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::UnixStream;

/// Where the daemon listens: `$ZAPPER_SOCKET`, else under `$XDG_RUNTIME_DIR`.
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("ZAPPER_SOCKET") {
        return p.into();
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/zapper-{}", unsafe { libc::getuid() })));
    base.join("zapper").join("api.sock")
}

pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new() -> Self {
        Self {
            socket: socket_path(),
        }
    }

    async fn conn(&self) -> Result<SendRequest<Full<Bytes>>> {
        let stream = UnixStream::connect(&self.socket).await.with_context(|| {
            format!(
                "connecting to the daemon at {} (is `zapper serve` running?)",
                self.socket.display()
            )
        })?;
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(conn.with_upgrades());
        Ok(sender)
    }

    fn request(method: Method, path: &str, body: Option<String>) -> Result<Request<Full<Bytes>>> {
        let mut b = Request::builder()
            .method(method)
            .uri(format!("http://zapper{path}"))
            .header("host", "zapper");
        if body.is_some() {
            b = b.header("content-type", "application/yaml");
        }
        Ok(b.body(Full::new(Bytes::from(body.unwrap_or_default())))?)
    }

    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<(StatusCode, Bytes)> {
        let mut sender = self.conn().await?;
        let resp = sender
            .send_request(Self::request(method, path, body)?)
            .await?;
        let status = resp.status();
        let bytes = resp.into_body().collect().await?.to_bytes();
        Ok((status, bytes))
    }

    fn check(status: StatusCode, bytes: &Bytes) -> Result<()> {
        if status.is_success() {
            return Ok(());
        }
        let msg = serde_json::from_slice::<Value>(bytes)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned());
        bail!("{msg}")
    }

    pub async fn json(&self, method: Method, path: &str, body: Option<String>) -> Result<Value> {
        let (status, bytes) = self.call(method, path, body).await?;
        Self::check(status, &bytes)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn text(&self, path: &str) -> Result<String> {
        let (status, bytes) = self.call(Method::GET, path, None).await?;
        Self::check(status, &bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A streaming GET, e.g. server-sent events.
    pub async fn stream(&self, path: &str) -> Result<Incoming> {
        let mut sender = self.conn().await?;
        let resp = sender
            .send_request(Self::request(Method::GET, path, None)?)
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let bytes = resp.into_body().collect().await?.to_bytes();
            Self::check(status, &bytes)?;
            bail!("{status}");
        }
        Ok(resp.into_body())
    }

    /// Opens a terminal in a session.
    pub async fn attach(
        &self,
        id: &str,
        tty: Option<(u16, u16)>,
        argv: &[String],
        system: bool,
    ) -> Result<TokioIo<hyper::upgrade::Upgraded>> {
        Ok(self.terminal(id, tty, argv, system, false, None).await?.0)
    }

    /// A terminal in session `id`: a new one, persistent if `persist`, or
    /// persistent terminal `attach` attached again. Also its number.
    pub async fn terminal(
        &self,
        id: &str,
        tty: Option<(u16, u16)>,
        argv: &[String],
        system: bool,
        persist: bool,
        attach: Option<u64>,
    ) -> Result<(TokioIo<hyper::upgrade::Upgraded>, Option<u64>)> {
        let mut q = Vec::new();
        if persist {
            q.push("persist=true".into());
        }
        if let Some(n) = attach {
            q.push(format!("terminal={n}"));
        }
        if let Some((r, c)) = tty {
            q.push(format!("tty={r}x{c}"));
        }
        if !argv.is_empty() {
            q.push(format!(
                "argv={}",
                url_encode(&serde_json::to_string(argv)?)
            ));
        }
        if system {
            q.push("system=true".into());
        }
        self.upgrade(&format!("/v1/sessions/{id}/attach?{}", q.join("&")))
            .await
    }

    /// A terminal in session `id` for the SSH gateway, as `client`: refused
    /// while the session's SSH is off, and closed when it is turned off.
    pub async fn ssh_terminal(
        &self,
        id: &str,
        tty: Option<(u16, u16)>,
        argv: &[String],
        env: &std::collections::BTreeMap<String, String>,
        client: Option<&str>,
    ) -> Result<TokioIo<hyper::upgrade::Upgraded>> {
        let mut q = vec!["ssh=true".to_string()];
        if let Some((r, c)) = tty {
            q.push(format!("tty={r}x{c}"));
        }
        if !argv.is_empty() {
            q.push(format!(
                "argv={}",
                url_encode(&serde_json::to_string(argv)?)
            ));
        }
        if !env.is_empty() {
            q.push(format!("env={}", url_encode(&serde_json::to_string(env)?)));
        }
        if let Some(c) = client {
            q.push(format!("client={}", url_encode(c)));
        }
        let path = format!("/v1/sessions/{id}/attach?{}", q.join("&"));
        Ok(self.upgrade(&path).await?.0)
    }

    /// Opens a terminal at `path`, upgraded to its frame stream; and its
    /// number, if the daemon gave one.
    async fn upgrade(
        &self,
        path: &str,
    ) -> Result<(TokioIo<hyper::upgrade::Upgraded>, Option<u64>)> {
        let mut sender = self.conn().await?;
        let req = Request::builder()
            .uri(format!("http://zapper{path}"))
            .header("host", "zapper")
            .header("connection", "upgrade")
            .header("upgrade", "zapper-shell")
            .body(Full::new(Bytes::new()))?;
        let resp = sender.send_request(req).await?;
        if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
            let status = resp.status();
            let bytes = resp.into_body().collect().await?.to_bytes();
            Self::check(status, &bytes)?;
            bail!("daemon did not open a terminal ({status})");
        }
        let n = resp
            .headers()
            .get("x-zapper-terminal")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        Ok((TokioIo::new(hyper::upgrade::on(resp).await?), n))
    }
}

pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
