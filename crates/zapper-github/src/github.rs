//! The GitHub API calls zapper makes: find an App's installation, mint a
//! token limited to a repository and permissions, list what an installation
//! reaches, and revoke a token.
//!
//! Errors carry GitHub's message and never a token or key.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};
use hyper::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app::{valid_login, App};
use crate::http;

/// Overrides the API base URL, for tests against a fake GitHub.
pub const API_ENV: &str = "ZAPPER_GITHUB_API_URL";

/// GitHub's permission levels, weakest first.
const LEVELS: [&str; 3] = ["read", "write", "admin"];

/// GitHub's limit on the repositories one token can name.
pub const MAX_REPOS: usize = 500;

/// What a token may do: the same permissions on each of one owner's
/// repositories. A token has one repository list and one permission map, so
/// every repository in it has every permission: put repositories together only
/// when they are meant to have the same access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub owner: String,
    /// Repository names without the owner, sorted, each once.
    pub repos: Vec<String>,
    /// App permission names to `read|write|admin`. `metadata: read` is added.
    pub permissions: BTreeMap<String, String>,
}

impl Scope {
    /// `owner/name` repositories (all of one owner) and permissions, checked.
    pub fn new<S: AsRef<str>>(repos: &[S], permissions: BTreeMap<String, String>) -> Result<Scope> {
        check_permissions(&permissions)?;
        let mut owner: Option<String> = None;
        let mut names: Vec<String> = Vec::new();
        for r in repos {
            let (o, name) = split_repo(r.as_ref())?;
            match &owner {
                Some(first) if !first.eq_ignore_ascii_case(&o) => bail!(
                    "all repositories must have one owner, since a token comes from one installation (got {first} and {o})"
                ),
                Some(_) => {}
                None => owner = Some(o),
            }
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                names.push(name);
            }
        }
        let owner = owner.ok_or_else(|| anyhow!("at least one repository is required"))?;
        if names.len() > MAX_REPOS {
            bail!(
                "a token can name at most {MAX_REPOS} repositories, got {}",
                names.len()
            );
        }
        names.sort_by_key(|n| n.to_ascii_lowercase());
        Ok(Scope {
            owner,
            repos: names,
            permissions,
        })
    }

    /// `owner/name` for each repository.
    pub fn full_names(&self) -> Vec<String> {
        self.repos
            .iter()
            .map(|r| format!("{}/{r}", self.owner))
            .collect()
    }
}

/// `owner/name`, each part a plausible GitHub name.
pub fn split_repo(repo: &str) -> Result<(String, String)> {
    let bad = || anyhow!("{repo:?} is not a repository (want OWNER/NAME)");
    let (owner, name) = repo.split_once('/').ok_or_else(bad)?;
    let ok_name = !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if !valid_login(owner) || !ok_name {
        return Err(bad());
    }
    Ok((owner.into(), name.into()))
}

/// Permission names and levels as the token endpoint takes them. At least
/// one is required: without any, GitHub grants every permission the App has.
pub fn check_permissions(p: &BTreeMap<String, String>) -> Result<()> {
    if p.is_empty() {
        bail!("at least one permission is required (otherwise GitHub grants every permission the App has)");
    }
    for (k, v) in p {
        let name_ok = !k.is_empty() && k.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
        if !name_ok || !LEVELS.contains(&v.as_str()) {
            bail!("bad permission {k}={v} (want name=read|write|admin)");
        }
    }
    Ok(())
}

/// A token and when it stops working (RFC 3339), as `zapper mint` prints it.
#[derive(Clone, Serialize, Deserialize)]
pub struct Token {
    pub value: String,
    pub expires_at: String,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Token(expires {})", self.expires_at)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Installation {
    pub id: u64,
    /// The account the App is installed on, as GitHub spells it.
    #[serde(default, deserialize_with = "account_login")]
    pub account: Option<String>,
    /// `all` or `selected`.
    #[serde(default)]
    pub repository_selection: String,
    /// What the App was granted on this installation: the ceiling for tokens.
    #[serde(default)]
    pub permissions: BTreeMap<String, String>,
}

/// `account: { login }` as the account's login.
fn account_login<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.and_then(|a| a["login"].as_str().map(str::to_string)))
}

pub struct GitHub {
    api: String,
}

impl Default for GitHub {
    fn default() -> Self {
        GitHub::new(std::env::var(API_ENV).ok().filter(|s| !s.is_empty()))
    }
}

impl GitHub {
    pub fn new(api: Option<String>) -> GitHub {
        GitHub {
            api: api
                .unwrap_or_else(|| "https://api.github.com".into())
                .trim_end_matches('/')
                .into(),
        }
    }

    /// The API base URL.
    pub fn api(&self) -> &str {
        &self.api
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        bearer: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let what = format!("{method} {path}");
        let r = http::send(method, &format!("{}{path}", self.api), Some(bearer), body)
            .await
            .with_context(|| what.clone())?;
        if !(200..300).contains(&r.status) {
            let text = String::from_utf8_lossy(&r.body);
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.trim().chars().take(300).collect());
            bail!("{what}: HTTP {}: {msg}", r.status);
        }
        if r.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&r.body).with_context(|| format!("{what}: decoding the response"))
    }

    /// A call with an installation token.
    pub(crate) async fn call_token(
        &self,
        method: Method,
        path: &str,
        token: &Token,
        body: Option<&Value>,
    ) -> Result<Value> {
        self.call(method, path, &token.value, body).await
    }

    fn jwt(app: &App) -> Result<String> {
        app.jwt(time::OffsetDateTime::now_utc().unix_timestamp())
    }

    /// The App's installation on a repository's owner (404: not installed).
    pub async fn installation_for_repo(&self, app: &App, scope_repo: &str) -> Result<Installation> {
        let (owner, name) = split_repo(scope_repo)?;
        let v = self
            .call(Method::GET, &format!("/repos/{owner}/{name}/installation"), &Self::jwt(app)?, None)
            .await
            .map_err(|e| {
                anyhow!("finding the App's installation on {owner}/{name} (is the App installed there?): {e}")
            })?;
        Ok(serde_json::from_value(v)?)
    }

    /// Where the App is installed. An App belongs to one account (a different
    /// account has its own App), so this is that account's installation, with
    /// its name as GitHub spells it. An App installed nowhere, or on several
    /// accounts, is an error that says so.
    pub async fn the_installation(&self, app: &App) -> Result<Installation> {
        let v = self
            .call(
                Method::GET,
                "/app/installations?per_page=100",
                &Self::jwt(app)?,
                None,
            )
            .await?;
        let mut all: Vec<Installation> = serde_json::from_value(v)?;
        match all.len() {
            0 => bail!("the GitHub App is not installed on any account: install it (https://github.com/apps/{}/installations/new)", app.slug.as_deref().unwrap_or("<app>")),
            1 => {
                let i = all.remove(0);
                if i.account.is_none() {
                    bail!("GitHub did not say which account the App is installed on");
                }
                Ok(i)
            }
            _ => bail!(
                "the GitHub App is installed on several accounts ({}); an App belongs to one account, so make one for each",
                all.iter().filter_map(|i| i.account.as_deref()).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// The App's installation on an account, which may be an organisation or
    /// a user. `None`: not installed there.
    pub async fn installation_for_owner(
        &self,
        app: &App,
        owner: &str,
    ) -> Result<Option<Installation>> {
        if !valid_login(owner) {
            bail!("{owner:?} is not a GitHub account name");
        }
        let jwt = Self::jwt(app)?;
        for kind in ["orgs", "users"] {
            match self
                .call(
                    Method::GET,
                    &format!("/{kind}/{owner}/installation"),
                    &jwt,
                    None,
                )
                .await
            {
                Ok(v) => return Ok(Some(serde_json::from_value(v)?)),
                Err(e) if format!("{e:#}").contains("HTTP 404") => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// A token for the scope's repositories with its permissions (plus
    /// `metadata: read`, which GitHub requires), from the installation on
    /// their owner. Every repository gets every permission.
    pub async fn mint(&self, app: &App, scope: &Scope) -> Result<Token> {
        let first = format!("{}/{}", scope.owner, scope.repos[0]);
        let inst = self.installation_for_repo(app, &first).await?;
        let mut perms = scope.permissions.clone();
        perms
            .entry("metadata".into())
            .or_insert_with(|| "read".into());
        let body = json!({ "repositories": scope.repos, "permissions": perms });
        let v = self
            .call(
                Method::POST,
                &format!("/app/installations/{}/access_tokens", inst.id),
                &Self::jwt(app)?,
                Some(&body),
            )
            .await?;
        let token = v["token"].as_str().unwrap_or_default();
        let expires = v["expires_at"].as_str().unwrap_or_default();
        if token.is_empty() || expires.is_empty() {
            bail!("GitHub returned no token");
        }
        // A token that quietly covers fewer repositories than asked for
        // would fail later and confusingly: say so now.
        if let Some(granted) = v["repositories"].as_array() {
            let has = |name: &str| {
                granted.iter().any(|r| {
                    r["name"]
                        .as_str()
                        .is_some_and(|n| n.eq_ignore_ascii_case(name))
                })
            };
            let missing: Vec<&str> = scope
                .repos
                .iter()
                .map(String::as_str)
                .filter(|n| !has(n))
                .collect();
            if !missing.is_empty() {
                bail!(
                    "GitHub's token does not cover {} (is the App installed on them?)",
                    missing.join(", ")
                );
            }
        }
        Ok(Token {
            value: token.into(),
            expires_at: expires.into(),
        })
    }

    /// The `owner/name` of every repository an installation token reaches.
    pub async fn repositories(&self, token: &Token) -> Result<Vec<String>> {
        let mut all = Vec::new();
        for page in 1..=20 {
            let v = self
                .call(
                    Method::GET,
                    &format!("/installation/repositories?per_page=100&page={page}"),
                    &token.value,
                    None,
                )
                .await?;
            let list = v["repositories"].as_array().cloned().unwrap_or_default();
            all.extend(
                list.iter()
                    .filter_map(|r| r["full_name"].as_str().map(str::to_string)),
            );
            if list.len() < 100 {
                break;
            }
        }
        Ok(all)
    }

    /// Invalidates an installation token at GitHub.
    pub async fn revoke(&self, token: &Token) -> Result<()> {
        self.call(Method::DELETE, "/installation/token", &token.value, None)
            .await
            .map(|_| ())
    }

    /// Every repository the installation reaches. Uses a token limited to
    /// `metadata: read` across the installation, and revokes it afterwards.
    pub async fn installation_repositories(
        &self,
        app: &App,
        installation: u64,
    ) -> Result<Vec<String>> {
        let body = json!({ "permissions": { "metadata": "read" } });
        let v = self
            .call(
                Method::POST,
                &format!("/app/installations/{installation}/access_tokens"),
                &Self::jwt(app)?,
                Some(&body),
            )
            .await?;
        let token = Token {
            value: v["token"].as_str().unwrap_or_default().into(),
            expires_at: v["expires_at"].as_str().unwrap_or_default().into(),
        };
        if token.value.is_empty() {
            bail!("GitHub returned no token");
        }
        let repos = self.repositories(&token).await;
        // The listing is done; a failed revoke only leaves a read-only
        // metadata token to lapse within the hour.
        let _ = self.revoke(&token).await;
        repos
    }

    /// An installation by id, with the permissions it was granted.
    pub async fn installation(&self, app: &App, id: u64) -> Result<Installation> {
        let v = self
            .call(
                Method::GET,
                &format!("/app/installations/{id}"),
                &Self::jwt(app)?,
                None,
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }
}

/// When a token expires, as unix seconds.
pub fn expires_unix(t: &Token) -> Result<i64> {
    time::OffsetDateTime::parse(
        &t.expires_at,
        &time::format_description::well_known::Rfc3339,
    )
    .map(|d| d.unix_timestamp())
    .map_err(|_| anyhow!("expires_at {:?} is not RFC 3339", t.expires_at))
}
