//! zapper's API, on its unix socket: the office is its operator. The same
//! client the workbench uses (proposal 13, §7 — a candidate for a crate both
//! share). Used by the `zapper-office` binary once the office runs as a
//! process of its own, in place of the in-process `Core`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
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

/// A client of zapper's API over its unix socket.
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

    /// `method path` with an optional JSON body, and what zapper answered.
    pub async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Answer> {
        let bytes = match body {
            Some(v) => Bytes::from(serde_json::to_vec(v)?),
            None => Bytes::new(),
        };
        let req = Request::builder()
            .method(method)
            .uri(format!("http://zapper{path}"))
            .header("host", "zapper")
            .header("content-type", "application/json")
            .body(Full::new(bytes))?;
        let resp = self.send(req).await?;
        let status = resp.status();
        let collected = resp.into_body().collect().await?.to_bytes();
        let body = if collected.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&collected).unwrap_or(Value::Null)
        };
        Ok(Answer { status, body })
    }

    /// A GET, returning the JSON if zapper said yes.
    pub async fn get(&self, path: &str) -> Result<Answer> {
        self.call(Method::GET, path, None).await
    }
}
