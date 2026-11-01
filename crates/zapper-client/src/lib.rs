//! A client of zapper's API over its unix socket. The workbench and the
//! office are both clients of the daemon (proposal 13, §7); this is the
//! client they share — `Zapper` (calls, streams, upgrades), where the socket
//! is, and `url_encode` for query values.

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

    /// `method path` with an optional JSON `body`, and what zapper answered.
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

    /// A GET, returning what zapper answered.
    pub async fn get(&self, path: &str) -> Result<Answer> {
        self.call(Method::GET, path, None).await
    }

    /// `GET path` as it comes: for a body that streams (server-sent events,
    /// an audit log), passed on as zapper sends it.
    pub async fn stream(&self, path: &str, headers: &HeaderMap) -> Result<Response<Incoming>> {
        self.send(Self::request(Method::GET, path, headers, Bytes::new())?)
            .await
    }

    /// `GET path` upgraded to a raw stream (a terminal, or a guest port): the
    /// daemon splices it to the guest. The stream, and the terminal's number
    /// if zapper gave one.
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

/// The largest first line a facility socket accepts from the daemon (the
/// caller line / MCP or ask request). The daemon sizes what it sends; this is
/// only a safety bound.
const MAX_LINE: usize = 1 << 20;

/// Reads the first line the daemon sends on a facility socket connection — a
/// byte at a time, so whatever follows (a guest's TLS-terminated stream) stays
/// in `stream` for the handler. `None` on a short or over-long line.
pub async fn read_line(stream: &mut UnixStream) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.len() < MAX_LINE {
        match stream.read(&mut b).await {
            Ok(1) if b[0] == b'\n' => return Some(line),
            Ok(1) => line.push(b[0]),
            _ => return None,
        }
    }
    None
}

/// Serves a facility's socket (the office's tools, decider, git server; the
/// workbench's facilities): for each connection, reads the daemon's first line
/// (the caller line, or an MCP/ask request) and hands `(line, stream)` to
/// `handler`, which parses the line and either replies on the stream or speaks
/// HTTP over it. Folds the accept loop all facilities repeat. The listener is
/// bound by the caller (under the daemon's `state/services/`); this converts
/// and serves it.
pub async fn serve_socket<H, Fut>(listener: std::os::unix::net::UnixListener, handler: H)
where
    H: Fn(Vec<u8>, UnixStream) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let Ok(listener) = tokio::net::UnixListener::from_std(listener) else {
        return;
    };
    while let Ok((mut stream, _)) = listener.accept().await {
        let handler = handler.clone();
        tokio::spawn(async move {
            if let Some(line) = read_line(&mut stream).await {
                handler(line, stream).await;
            }
        });
    }
}

/// `s` as a URL's query value (percent-encoded).
pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect()
}
