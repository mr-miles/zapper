//! Support for the office's own command line (`zapper-office firm/mail/drive`,
//! in `firm_cli.rs`): a small client of the running office's unix-socket API
//! and the output helpers the commands share. The office serves its firm
//! routes to the browser (behind the UI gateway, at `/api`) and, for this CLI,
//! on a local unix socket of its own (`standalone::office_socket_path`), the
//! way the daemon serves the `zapper` CLI — so these commands reach the office
//! the way `zapper`'s reached the daemon. The socket plumbing mirrors
//! [`zapper_client::Zapper`], but keeps raw bytes (a drive download is binary)
//! and sends a string body.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::UnixStream;

pub use zapper_client::url_encode;

use zapper_office::standalone::office_socket_path;

/// A client of the running office's unix-socket API, as the board.
pub struct Client {
    socket: std::path::PathBuf,
}

impl Client {
    pub fn new() -> Self {
        Self {
            socket: office_socket_path(),
        }
    }

    async fn conn(&self) -> Result<SendRequest<Full<Bytes>>> {
        let stream = UnixStream::connect(&self.socket).await.with_context(|| {
            format!(
                "connecting to the office at {} (is `zapper-office serve` running?)",
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
}

/// The config file zapper and the office read: `$ZAPPER_CONFIG`, else the
/// checkout's `config/zapper.yaml`, else `~/.config/zapper/zapper.yaml` — the
/// same file the daemon reads. Sets CONFIG_DIR to its directory so relative
/// paths (a firm's `agents_dir`/`skills_dir`, the daemon's `state`) resolve
/// against it, as they do for the daemon.
pub fn config_path() -> std::path::PathBuf {
    use zapper_policy::expand_home;
    let path = match std::env::var("ZAPPER_CONFIG") {
        Ok(p) => std::path::PathBuf::from(expand_home(&p)),
        Err(_) => {
            let local = std::path::PathBuf::from("config/zapper.yaml");
            if local.is_file() {
                local
            } else {
                std::path::PathBuf::from(expand_home("~/.config/zapper/zapper.yaml"))
            }
        }
    };
    let path = std::path::absolute(&path).unwrap_or(path);
    if let Some(dir) = path.parent() {
        zapper_policy::set_config_dir(dir);
    }
    path
}

/// The directory the config file lives in: where the office's workers' agents
/// and skills are installed, beside the config (as `zapper init` wrote them).
pub fn config_dir() -> std::path::PathBuf {
    config_path()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf()
}

/// The daemon's `state` directory, from its config file. Used by `firm replay`
/// to find the daemon's `ui-url`.
pub fn daemon_state_dir() -> Result<std::path::PathBuf> {
    use zapper_policy::{expand_home, Config};
    let path = config_path();
    let config = if path.exists() {
        Config::from_yaml(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("loading {}", path.display()))?
    } else {
        Config::default()
    };
    Ok(std::path::PathBuf::from(expand_home(&config.daemon.state)))
}

/// Prints `v` as pretty JSON.
pub fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

/// A JSON value as a plain string (a bare string without its quotes).
pub fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Prints rows as aligned columns.
pub fn table(header: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let line = |cells: Vec<String>| {
        let s: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
            .collect();
        println!("{}", s.join("  ").trim_end());
    };
    line(header.iter().map(|h| h.to_uppercase()).collect());
    for r in rows {
        line(r);
    }
}
