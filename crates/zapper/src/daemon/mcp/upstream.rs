//! Connections from the gateway to the servers behind it: remote endpoints
//! over streamable HTTP, and programs the daemon starts, over stdio. Both
//! speak JSON-RPC; a server is initialised when it is first used.

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{HeaderName, HeaderValue, CONTENT_TYPE, HOST};
use hyper::{Request, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio_rustls::TlsConnector;
use zapper_policy::{render, McpServer};

use crate::daemon::tls;

/// The protocol version the gateway asks servers for; they answer with the
/// one they speak.
pub const PROTOCOL: &str = "2025-06-18";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const INIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest response taken from a server.
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
/// Largest line taken from a stdio server.
const MAX_LINE: usize = 16 * 1024 * 1024;

/// A JSON-RPC error, from a server or made by the gateway.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }

    pub fn from_json(e: &Value) -> Self {
        RpcError {
            code: e["code"].as_i64().unwrap_or(-32603),
            message: e["message"].as_str().unwrap_or("error").to_string(),
            data: e.get("data").cloned(),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut v = json!({ "code": self.code, "message": self.message });
        if let Some(d) = &self.data {
            v["data"] = d.clone();
        }
        v
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL,
        "capabilities": {},
        "clientInfo": { "name": "zapper-gateway", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// A connected server.
pub enum Upstream {
    Http(HttpUpstream),
    Stdio(StdioUpstream),
}

impl Upstream {
    /// Connects to `def` and initialises it. `secret` gives the value of a
    /// secret for headers and the environment.
    pub async fn connect(
        def: &McpServer,
        secret: &(dyn Fn(&str) -> Result<String, String> + Sync),
    ) -> Result<Upstream, RpcError> {
        let rendered = |template: &str| -> Result<String, RpcError> {
            let mut err = None;
            let v = render(template, |n| match secret(n) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(e);
                    String::new()
                }
            });
            err.map_or(Ok(v), |e| Err(RpcError::internal(e)))
        };
        let up = if let Some(url) = &def.url {
            let mut headers = Vec::new();
            for (k, v) in &def.headers {
                headers.push((k.clone(), rendered(v)?));
            }
            Upstream::Http(HttpUpstream::new(url, headers)?)
        } else {
            let mut env = BTreeMap::new();
            for (k, v) in &def.env {
                env.insert(k.clone(), rendered(v)?);
            }
            Upstream::Stdio(StdioUpstream::spawn(
                &def.command,
                &env,
                def.cwd.as_deref(),
            )?)
        };
        tokio::time::timeout(INIT_TIMEOUT, up.initialize())
            .await
            .map_err(|_| RpcError::internal("timed out initialising"))??;
        Ok(up)
    }

    async fn initialize(&self) -> Result<(), RpcError> {
        let r = self.request_raw("initialize", initialize_params()).await?;
        if let (Upstream::Http(h), Some(v)) = (self, r["protocolVersion"].as_str()) {
            *h.protocol.lock().unwrap() = Some(v.to_string());
        }
        self.notify("notifications/initialized").await
    }

    /// Sends a request and waits for its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match self {
            // A server that forgot the session (a restart) gets one new
            // handshake.
            Upstream::Http(h) => match h.request(method, params.clone()).await {
                Err(e) if e.code == SESSION_GONE => {
                    h.reset();
                    self.initialize().await?;
                    h.request(method, params).await
                }
                r => r,
            },
            Upstream::Stdio(_) => self.request_raw(method, params).await,
        }
    }

    async fn request_raw(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match self {
            Upstream::Http(h) => h.request(method, params).await,
            Upstream::Stdio(s) => s.request(method, params).await,
        }
    }

    pub async fn notify(&self, method: &str) -> Result<(), RpcError> {
        match self {
            Upstream::Http(h) => h.notify(method).await,
            Upstream::Stdio(s) => s.notify(method).await,
        }
    }

    /// False once the server's process has gone.
    pub fn alive(&self) -> bool {
        match self {
            Upstream::Http(_) => true,
            Upstream::Stdio(s) => s.alive(),
        }
    }

    /// Whether the server said its lists changed.
    pub fn take_stale(&self) -> bool {
        match self {
            Upstream::Http(_) => false,
            Upstream::Stdio(s) => s.stale.swap(false, Ordering::Relaxed),
        }
    }
}

// --- streamable HTTP ---------------------------------------------------

/// Code of the error for a server that no longer knows our session.
const SESSION_GONE: i64 = -32001;

pub struct HttpUpstream {
    tls: bool,
    host: String,
    port: u16,
    target: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
    next_id: AtomicI64,
}

impl HttpUpstream {
    fn new(url: &str, headers: Vec<(String, String)>) -> Result<Self, RpcError> {
        let uri: Uri = url
            .parse()
            .map_err(|e| RpcError::internal(format!("url {url}: {e}")))?;
        let tls = uri.scheme_str() == Some("https");
        let host = uri
            .host()
            .ok_or_else(|| RpcError::internal(format!("url {url}: no host")))?
            .trim_matches(['[', ']'])
            .to_string();
        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
        let target = uri.path_and_query().map_or("/", |p| p.as_str()).to_string();
        let mut parsed = Vec::new();
        for (k, v) in headers {
            parsed.push((
                HeaderName::from_bytes(k.as_bytes())
                    .map_err(|_| RpcError::internal(format!("bad header name {k}")))?,
                HeaderValue::from_str(&v)
                    .map_err(|_| RpcError::internal(format!("bad value for header {k}")))?,
            ));
        }
        Ok(HttpUpstream {
            tls,
            host,
            port,
            target,
            headers: parsed,
            session: Mutex::new(None),
            protocol: Mutex::new(None),
            next_id: AtomicI64::new(1),
        })
    }

    fn reset(&self) {
        *self.session.lock().unwrap() = None;
        *self.protocol.lock().unwrap() = None;
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let msg = self
            .post(&body, Some(id))
            .await?
            .ok_or_else(|| RpcError::internal("the server sent no response"))?;
        match msg.get("error") {
            Some(e) => Err(RpcError::from_json(e)),
            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
        }
    }

    async fn notify(&self, method: &str) -> Result<(), RpcError> {
        let body = json!({ "jsonrpc": "2.0", "method": method });
        self.post(&body, None).await.map(|_| ())
    }

    async fn dial(&self) -> Result<Box<dyn Io>, RpcError> {
        let connect = async {
            let tcp = TcpStream::connect((self.host.as_str(), self.port)).await?;
            let _ = tcp.set_nodelay(true);
            std::io::Result::Ok(tcp)
        };
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| RpcError::internal(format!("connecting to {}: timed out", self.host)))?
            .map_err(|e| RpcError::internal(format!("connecting to {}: {e}", self.host)))?;
        if !self.tls {
            return Ok(Box::new(tcp));
        }
        let name = rustls::pki_types::ServerName::try_from(self.host.clone())
            .map_err(|e| RpcError::internal(format!("{}: {e}", self.host)))?;
        let tls = TlsConnector::from(tls::client_config())
            .connect(name, tcp)
            .await
            .map_err(|e| RpcError::internal(format!("TLS to {}: {e}", self.host)))?;
        Ok(Box::new(tls))
    }

    /// POSTs a message. With `want` set, returns the response carrying that
    /// id, from a JSON body or an event stream; otherwise `None`.
    async fn post(&self, body: &Value, want: Option<i64>) -> Result<Option<Value>, RpcError> {
        let io = self.dial().await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
            .await
            .map_err(|e| RpcError::internal(format!("{}: {e}", self.host)))?;
        tokio::spawn(conn);
        let session = self.session.lock().unwrap().clone();
        let host = if (self.tls && self.port == 443) || (!self.tls && self.port == 80) {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        };
        let mut req = Request::post(&self.target)
            .header(HOST, host)
            .header(CONTENT_TYPE, "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("connection", "close");
        if let Some(p) = self.protocol.lock().unwrap().as_deref() {
            req = req.header("mcp-protocol-version", p);
        }
        if let Some(s) = &session {
            req = req.header("mcp-session-id", s.as_str());
        }
        let mut req = req
            .body(Full::new(Bytes::from(body.to_string())))
            .map_err(|e| RpcError::internal(e.to_string()))?;
        for (k, v) in &self.headers {
            req.headers_mut().insert(k.clone(), v.clone());
        }
        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| RpcError::internal(format!("{}: {e}", self.host)))?;
        let (parts, mut incoming) = resp.into_parts();
        if let Some(s) = parts
            .headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().unwrap() = Some(s.to_string());
        }
        match parts.status {
            StatusCode::ACCEPTED | StatusCode::NO_CONTENT => return Ok(None),
            StatusCode::NOT_FOUND if session.is_some() => {
                return Err(RpcError::new(SESSION_GONE, "the server dropped the session"))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(RpcError::internal(format!(
                    "{} refused the gateway ({}); check the headers and secrets of its mcp.servers entry",
                    self.host, parts.status
                )))
            }
            s if !s.is_success() => {
                let text = read_body(&mut incoming, 512).await.unwrap_or_default();
                return Err(RpcError::internal(format!(
                    "{} answered {s}: {}",
                    self.host,
                    String::from_utf8_lossy(&text).trim()
                )));
            }
            _ => {}
        }
        let Some(want) = want else {
            return Ok(None);
        };
        let ctype = parts
            .headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ctype.starts_with("text/event-stream") {
            let mut events = Events::default();
            while let Some(frame) = incoming.frame().await {
                let frame = frame.map_err(|e| RpcError::internal(format!("{}: {e}", self.host)))?;
                if let Ok(data) = frame.into_data() {
                    for msg in events.push(&data)? {
                        if msg["id"].as_i64() == Some(want) && msg.get("method").is_none() {
                            return Ok(Some(msg));
                        }
                    }
                }
            }
            Err(RpcError::internal(
                "the event stream ended without a response",
            ))
        } else {
            let bytes = read_body(&mut incoming, MAX_RESPONSE).await?;
            let msg: Value = serde_json::from_slice(&bytes)
                .map_err(|e| RpcError::internal(format!("{}: not JSON: {e}", self.host)))?;
            // A batch: the response is the element with our id.
            Ok(match msg {
                Value::Array(a) => a.into_iter().find(|m| m["id"].as_i64() == Some(want)),
                m => Some(m),
            })
        }
    }
}

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

async fn read_body(body: &mut hyper::body::Incoming, max: usize) -> Result<Vec<u8>, RpcError> {
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| RpcError::internal(e.to_string()))?;
        if let Ok(data) = frame.into_data() {
            out.extend_from_slice(&data);
            if out.len() > max {
                // Error bodies are only quoted; real ones must fit.
                if max < 4096 {
                    out.truncate(max);
                    break;
                }
                return Err(RpcError::internal("the response is too large"));
            }
        }
    }
    Ok(out)
}

/// Incremental parser of a `text/event-stream`: the JSON of each event's
/// `data` lines.
#[derive(Default)]
struct Events {
    buf: Vec<u8>,
}

impl Events {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>, RpcError> {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > MAX_RESPONSE {
            return Err(RpcError::internal("the response is too large"));
        }
        let mut out = Vec::new();
        loop {
            let text = String::from_utf8_lossy(&self.buf).replace("\r\n", "\n");
            let Some(end) = text.find("\n\n") else { break };
            let event = text[..end].to_string();
            let rest = text[end + 2..].to_string();
            self.buf = rest.into_bytes();
            let data: Vec<&str> = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            if data.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&data.join("\n")) {
                out.push(v);
            }
        }
        Ok(out)
    }
}

// --- stdio -------------------------------------------------------------

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

pub struct StdioUpstream {
    stdin: AsyncMutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicI64,
    alive: Arc<AtomicBool>,
    stale: Arc<AtomicBool>,
    /// The last lines the program wrote to stderr, for error messages.
    stderr: Arc<Mutex<Vec<String>>>,
    // Killed with the connection.
    _child: Child,
}

impl StdioUpstream {
    fn spawn(
        command: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&str>,
    ) -> Result<Self, RpcError> {
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for var in ["PATH", "HOME", "LANG", "TMPDIR"] {
            if let Ok(v) = std::env::var(var) {
                cmd.env(var, v);
            }
        }
        cmd.envs(env);
        if let Some(cwd) = cwd {
            cmd.current_dir(zapper_policy::expand_home(cwd));
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| RpcError::internal(format!("starting {}: {e}", command[0])))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr_pipe = child.stderr.take().expect("piped");
        let pending: Pending = Default::default();
        let (alive, stale) = (
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
        );
        let stderr: Arc<Mutex<Vec<String>>> = Default::default();
        {
            let stderr = stderr.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr_pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut last = stderr.lock().unwrap();
                    last.push(line.chars().take(300).collect());
                    if last.len() > 10 {
                        last.remove(0);
                    }
                }
            });
        }
        {
            let (pending, alive, stale) = (pending.clone(), alive.clone(), stale.clone());
            let stderr = stderr.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match (&mut reader)
                        .take(MAX_LINE as u64 + 1)
                        .read_until(b'\n', &mut line)
                        .await
                    {
                        Ok(0) | Err(_) => break,
                        Ok(_) if line.len() > MAX_LINE => break,
                        Ok(_) => {}
                    }
                    let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
                        continue;
                    };
                    if msg.get("method").is_some() {
                        if msg["method"]
                            .as_str()
                            .is_some_and(|m| m.ends_with("list_changed"))
                        {
                            stale.store(true, Ordering::Relaxed);
                        }
                        continue;
                    }
                    let Some(id) = msg["id"].as_i64() else {
                        continue;
                    };
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(match msg.get("error") {
                            Some(e) => Err(RpcError::from_json(e)),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        });
                    }
                }
                alive.store(false, Ordering::Relaxed);
                let last = stderr.lock().unwrap().last().cloned().unwrap_or_default();
                let why = if last.is_empty() {
                    "the server exited".to_string()
                } else {
                    format!("the server exited: {last}")
                };
                for (_, tx) in pending.lock().unwrap().drain() {
                    let _ = tx.send(Err(RpcError::internal(why.clone())));
                }
            });
        }
        Ok(StdioUpstream {
            stdin: AsyncMutex::new(stdin),
            pending,
            next_id: AtomicI64::new(1),
            alive,
            stale,
            stderr,
            _child: child,
        })
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    async fn send(&self, msg: &Value) -> Result<(), RpcError> {
        if !self.alive() {
            return Err(RpcError::internal("the server has exited"));
        }
        let mut line = msg.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| RpcError::internal(format!("writing to the server: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| RpcError::internal(format!("writing to the server: {e}")))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.send(&msg).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match rx.await {
            Ok(r) => r,
            Err(_) => {
                let last = self
                    .stderr
                    .lock()
                    .unwrap()
                    .last()
                    .cloned()
                    .unwrap_or_default();
                Err(RpcError::internal(format!("the server exited {last}")))
            }
        }
    }

    async fn notify(&self, method: &str) -> Result<(), RpcError> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method }))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_streams() {
        let mut e = Events::default();
        let a = e.push(b"event: message\ndata: {\"id\":1,").unwrap();
        assert!(a.is_empty());
        let b = e
            .push(b"\"result\":{}}\r\n\r\n: comment\n\ndata: {\"method\":\"x\"}\n\n")
            .unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0]["id"], 1);
        assert_eq!(b[1]["method"], "x");
        // Multi-line data joins with newlines.
        let c = e.push(b"data: [1,\ndata: 2]\n\n").unwrap();
        assert_eq!(c, [json!([1, 2])]);
    }
}
