//! A GitHub App's credentials file and the JWT it signs.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use serde::Deserialize;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A GitHub account name. It names the credentials file, so it must not hold
/// a path separator.
pub fn valid_login(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 39
        && !s.starts_with('-')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The directory of the App credentials files, one `<owner>.yaml` each.
pub fn apps_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/nonexistent"));
    base.join("zapper").join("github-apps")
}

/// The credentials file of the App that lives in `owner`'s account. GitHub
/// logins are case-insensitive, so the name is lowercased.
pub fn owner_file(owner: &str) -> Result<PathBuf> {
    if !valid_login(owner) {
        bail!("{owner:?} is not a GitHub account name");
    }
    Ok(apps_dir().join(format!("{}.yaml", owner.to_ascii_lowercase())))
}

/// A credentials file named in a config: a plain file name `<name>.yaml`
/// (letters, digits, `-`, `_` and `.`), taken as it is, in [`apps_dir`]. No
/// path, so a config can't point outside that directory.
pub fn app_file(name: &str) -> Result<PathBuf> {
    let stem = name.strip_suffix(".yaml").unwrap_or("");
    let ok = !stem.is_empty()
        && name.len() <= 100
        && !stem.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    if !ok {
        bail!(
            "{name:?} is not a credentials file name: a plain NAME.yaml in {}",
            apps_dir().display()
        );
    }
    Ok(apps_dir().join(name))
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Github {
    app_id: String,
    client_id: String,
    app_slug: String,
    private_key: String,
    private_key_path: String,
    /// Where the App's GitHub is: its API and its site (git and links), for
    /// GitHub Enterprise Server; github.com's if unset.
    api_url: String,
    web_url: String,
}

/// github.com's API and site: an App's unless its file names others.
pub const API_URL: &str = "https://api.github.com";
pub const WEB_URL: &str = "https://github.com";

#[derive(Debug, Deserialize, Default)]
struct File {
    #[serde(default)]
    github: Github,
}

/// An App's signing credentials. The private key never leaves this struct:
/// there is no accessor for it, and `Debug` leaves it out.
pub struct App {
    /// The JWT `iss`: the client ID, or else the numeric App ID.
    issuer: String,
    /// The App's URL name, if the file has it (for the install link).
    pub slug: Option<String>,
    /// Its GitHub's API base, and its site's: `apiUrl` and `webUrl` in the
    /// file, else github.com's.
    pub api_url: String,
    pub web_url: String,
    key: RsaKeyPair,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("issuer", &self.issuer)
            .field("slug", &self.slug)
            .finish_non_exhaustive()
    }
}

impl App {
    /// Reads a credentials file. Warnings (a loose file mode) go to `warn`.
    pub fn load(path: &Path, warn: &mut dyn FnMut(String)) -> Result<App> {
        check_mode(path, warn);
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let f: File = serde_yaml::from_str(&text).with_context(|| format!("{}", path.display()))?;
        let g = f.github;
        let issuer = if g.client_id.is_empty() {
            g.app_id
        } else {
            g.client_id
        };
        if issuer.is_empty() {
            bail!(
                "{}: github.clientId or github.appId is required",
                path.display()
            );
        }
        let pem = match (g.private_key.is_empty(), g.private_key_path.is_empty()) {
            (false, false) => bail!(
                "{}: set github.privateKey or github.privateKeyPath, not both",
                path.display()
            ),
            (true, true) => bail!(
                "{}: github.privateKey or github.privateKeyPath is required",
                path.display()
            ),
            (false, true) => g.private_key,
            (true, false) => {
                let p = Path::new(&g.private_key_path);
                if !p.is_absolute() {
                    bail!("{}: github.privateKeyPath must be absolute", path.display());
                }
                check_mode(p, warn);
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?
            }
        };
        let key = parse_key(&pem).with_context(|| format!("{}", path.display()))?;
        // https, or plain http to this machine; the site may also be a
        // local directory of repositories (`file://`), for its git.
        let url = |key: &str, u: String, default: &str, file_ok: bool| -> Result<String> {
            let u = if u.is_empty() { default.to_string() } else { u };
            let u = u.trim_end_matches('/').to_string();
            if !(file_ok && u.starts_with("file:///")) {
                crate::http::split(&u)
                    .with_context(|| format!("{}: github.{key}", path.display()))?;
            }
            Ok(u)
        };
        Ok(App {
            issuer,
            slug: Some(g.app_slug).filter(|s| !s.is_empty()),
            api_url: url("apiUrl", g.api_url, API_URL, false)?,
            web_url: url("webUrl", g.web_url, WEB_URL, true)?,
            key,
        })
    }

    /// Builds an App from a PEM key, for a freshly created App.
    pub fn from_pem(issuer: &str, slug: Option<&str>, pem: &str) -> Result<App> {
        Ok(App {
            issuer: issuer.into(),
            slug: slug.map(str::to_string),
            api_url: API_URL.into(),
            web_url: WEB_URL.into(),
            key: parse_key(pem)?,
        })
    }

    /// A JWT that authenticates as the App. GitHub allows at most ten
    /// minutes, and `iat` is backdated a minute for clock skew.
    pub fn jwt(&self, now: i64) -> Result<String> {
        let header = B64.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims =
            serde_json::json!({ "iat": now - 60, "exp": now + 9 * 60, "iss": self.issuer });
        let unsigned = format!("{header}.{}", B64.encode(serde_json::to_vec(&claims)?));
        let mut sig = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                unsigned.as_bytes(),
                &mut sig,
            )
            .map_err(|_| anyhow!("signing the JWT failed"))?;
        Ok(format!("{unsigned}.{}", B64.encode(sig)))
    }
}

fn check_mode(path: &Path, warn: &mut dyn FnMut(String)) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = std::fs::metadata(path) {
        let mode = m.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            warn(format!(
                "warning: {} is mode {mode:o}; it holds the App's private key, chmod 600 it",
                path.display()
            ));
        }
    }
}

/// An RSA key in PEM, PKCS#1 (`RSA PRIVATE KEY`, as GitHub issues it) or
/// PKCS#8 (`PRIVATE KEY`).
fn parse_key(pem: &str) -> Result<RsaKeyPair> {
    let mut label = None;
    let mut body = String::new();
    for line in pem.lines().map(str::trim) {
        if let Some(l) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|l| l.strip_suffix("-----"))
        {
            label = Some(l.to_string());
        } else if line.starts_with("-----") || line.is_empty() {
            continue;
        } else if label.is_some() {
            body.push_str(line);
        }
    }
    let label = label.ok_or_else(|| {
        anyhow!("private key is not PEM (check its indentation under privateKey: |)")
    })?;
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .map_err(|_| anyhow!("private key is not valid base64 (check its indentation)"))?;
    let key = match label.as_str() {
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
        other => bail!("private key is a {other:?}, not an RSA private key"),
    };
    key.map_err(|e| anyhow!("parsing the private key: {e}"))
}
