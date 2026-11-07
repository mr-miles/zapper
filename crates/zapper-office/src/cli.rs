//! Support for the office's own command line (`zapper-office firm/mail/drive`,
//! in `firm_cli.rs`): a small client of the running office's unix-socket API
//! and the output helpers the commands share. The office serves its firm
//! routes to the browser (behind the UI gateway, at `/api`) and, for this CLI,
//! on a local unix socket of its own (`standalone::office_socket_path`), the
//! way the daemon serves the `zapper` CLI — so these commands reach the office
//! the way `zapper`'s reached the daemon.
//!
//! The socket plumbing is the shared [`zapper_client::Zapper`]; [`Client`] is
//! a thin board-facing adapter over it, keeping raw response bytes (a drive
//! download is binary) and the board's error wrapping.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{HeaderMap, Method, StatusCode};
use serde_json::Value;
use zapper_client::Zapper;

pub use zapper_client::url_encode;

use zapper_office::standalone::office_socket_path;

/// A client of the running office's unix-socket API, as the board: the shared
/// [`Zapper`] (the socket plumbing) plus the board's JSON/text/bytes helpers
/// and error wrapping.
pub struct Client(Zapper);

impl Client {
    pub fn new() -> Self {
        Self(Zapper::new(office_socket_path()))
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
        self.0.call_raw(method, path, ct, bytes).await
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
        let resp = self.0.stream(path, &HeaderMap::new()).await?;
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

/// The office's own config file (`office.yaml`): the firms it runs, beside the
/// daemon's `zapper.yaml` (`$ZAPPER_OFFICE_CONFIG` overrides). Firms are the
/// office's config, not the daemon's (proposal 13 decision 2).
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
