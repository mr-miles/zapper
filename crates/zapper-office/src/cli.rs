//! Support for the office's own command line (`zapper-office firm/mail/drive`,
//! in `firm_cli.rs`): a small client of the running office's unix-socket API
//! and the output helpers the commands share. The office serves its firm
//! routes to the browser (behind the UI gateway, at `/api`) and, for this CLI,
//! on a local unix socket of its own (`standalone::office_socket_path`),
//! which these commands reach.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{HeaderMap, Method, StatusCode};
use serde_json::Value;

use zapper_office::standalone::office_socket_path;

/// A client of the running office's unix-socket API, as the board: raw
/// response bytes (a drive download is binary), and the board's JSON and text
/// helpers and error wrapping.
pub struct Client(std::path::PathBuf);

impl Client {
    pub fn new() -> Self {
        Self(office_socket_path())
    }

    /// `method path` with `headers` and `body`, on a connection of its own.
    async fn send(
        &self,
        method: Method,
        path: &str,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<hyper::Response<Incoming>> {
        let stream = tokio::net::UnixStream::connect(&self.0)
            .await
            .with_context(|| {
                format!(
                    "the office at {} (is `zapper-office serve` running?)",
                    self.0.display()
                )
            })?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
        tokio::spawn(conn);
        let mut b = hyper::Request::builder()
            .method(method)
            .uri(format!("http://office{path}"))
            .header("host", "office");
        for (k, v) in headers {
            b = b.header(k, v);
        }
        Ok(sender
            .send_request(b.body(http_body_util::Full::new(body))?)
            .await?)
    }

    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<(StatusCode, Bytes)> {
        // The office handlers read the raw body, so the content-type is only a
        // label; the board's commands send JSON.
        let (ct, bytes) = match body {
            Some(b) => (Some("application/json"), Bytes::from(b)),
            None => (None, Bytes::new()),
        };
        let mut headers = HeaderMap::new();
        if let Some(ct) = ct {
            headers.insert("content-type", ct.parse()?);
        }
        let resp = self.send(method, path, &headers, bytes).await?;
        let status = resp.status();
        Ok((status, resp.into_body().collect().await?.to_bytes()))
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
        let resp = self
            .send(Method::GET, path, &HeaderMap::new(), Bytes::new())
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

/// The config file the office reads (the profiles and image recipes):
/// `$ZAPPER_CONFIG`, else the checkout's `config/zapper.yaml`, else
/// `~/.config/zapper/zapper.yaml`. Sets CONFIG_DIR to its directory so
/// relative paths (a firm's `agents_dir`/`skills_dir`, `daemon.state`)
/// resolve against it.
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
/// and skills are installed, beside the config (as `zapper-office init` writes them).
pub fn config_dir() -> std::path::PathBuf {
    config_path()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf()
}

/// The office's own config file (`office.yaml`): the firms it runs, beside
/// `zapper.yaml` (`$ZAPPER_OFFICE_CONFIG` overrides).
pub fn office_config_path() -> std::path::PathBuf {
    use zapper_policy::expand_home;
    match std::env::var("ZAPPER_OFFICE_CONFIG") {
        Ok(p) => {
            let path = std::path::PathBuf::from(expand_home(&p));
            std::path::absolute(&path).unwrap_or(path)
        }
        Err(_) => config_dir().join("office.yaml"),
    }
}

/// The office's `state` directory, from `zapper.yaml` (`daemon.state`): where
/// firms' runs are kept, and the dashboard's URL for `firm replay`.
pub fn state_dir() -> Result<std::path::PathBuf> {
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
