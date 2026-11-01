//! The secrets facility: values the workbench keeps for zapper, and their
//! entries in zapper's config. A value is a file of its own under
//! `~/.config/zapper-workbench/secrets/` (mode 0600 in a 0700 directory);
//! zapper's entry names it as a `file` source, with the hosts it may go to
//! and how it reaches a session (`proxy` by default), so zapper reads it on
//! each use and the config never holds it. Values are never shown back:
//! the facility lists names, modes, hosts and what uses each.
//!
//! It changes only the secrets it made (those whose file is in its
//! directory): one defined in the config file by hand stays the operator's.
//! Changes go to zapper's running config, one secret at a time; saving them
//! to the startup file is its own, explicit action.

use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Extension, Path as UrlPath, State};
use axum::http::{Method, StatusCode};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::facility::{Facility, Panel};
use crate::gateway::{Ctx, Error};
use crate::tokens::{Grant, Right};

/// Longest value kept: credentials, not files.
const MAX_VALUE: usize = 64 * 1024;

/// A kind of credential the workbench knows: where it goes, how it reaches
/// a session, the variable a profile usually puts it in, and what a value of
/// it looks like.
pub struct Preset {
    pub id: &'static str,
    pub title: &'static str,
    pub hosts: &'static [&'static str],
    pub mode: &'static str,
    /// The variable a profile gives it as, `{{secret:NAME}}`.
    pub env: &'static str,
    pub hint: &'static str,
    check: fn(&str) -> Result<(), String>,
}

fn prefixed(v: &str, prefixes: &[&str], what: &str) -> Result<(), String> {
    if prefixes.iter().any(|p| v.starts_with(p)) {
        Ok(())
    } else {
        Err(format!("{what} starts with {}", prefixes.join(" or ")))
    }
}

/// An AWS secret's value: credential_process JSON.
fn aws_json(v: &str) -> Result<(), String> {
    let j: Value = serde_json::from_str(v).map_err(|_| {
        "an AWS secret is JSON: {\"AccessKeyId\", \"SecretAccessKey\", \"SessionToken\"?}"
            .to_string()
    })?;
    if j["AccessKeyId"].is_string() && j["SecretAccessKey"].is_string() {
        Ok(())
    } else {
        Err("an AWS secret has AccessKeyId and SecretAccessKey".into())
    }
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "claude",
        title: "Claude",
        hosts: &["api.anthropic.com"],
        mode: "proxy",
        env: "CLAUDE_CODE_OAUTH_TOKEN",
        hint: "`claude setup-token` prints one (sk-ant-oat…); an API key (sk-ant-api…) goes in ANTHROPIC_API_KEY instead",
        check: |v| prefixed(v, &["sk-ant-"], "a Claude token"),
    },
    Preset {
        id: "openrouter",
        title: "OpenRouter",
        hosts: &["openrouter.ai"],
        mode: "proxy",
        env: "OPENROUTER_API_KEY",
        hint: "a key from openrouter.ai/settings/keys (sk-or-v1-…)",
        check: |v| prefixed(v, &["sk-or-"], "an OpenRouter key"),
    },
    Preset {
        id: "github",
        title: "GitHub token",
        hosts: &["api.github.com", "github.com"],
        mode: "proxy",
        env: "GITHUB_TOKEN",
        hint: "a fine-grained token (github_pat_…), scoped to the repositories it needs",
        check: |v| {
            prefixed(
                v,
                &["github_pat_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
                "a GitHub token",
            )
        },
    },
    Preset {
        id: "aws",
        title: "AWS",
        hosts: &["*.amazonaws.com"],
        mode: "aws",
        env: "AWS_ACCESS_KEY_ID",
        hint: "credential_process JSON; the proxy signs on the host, the guest holds a fake key",
        check: aws_json,
    },
    Preset {
        id: "database",
        title: "Database",
        hosts: &[],
        mode: "dbauth",
        env: "",
        hint: "a database password: give its host and the user it authenticates as",
        check: |_| Ok(()),
    },
];

fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// A secret's name: as `{{secret:NAME}}` writes it, and short.
fn checked_name(name: &str) -> Result<&str, Error> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(name)
    } else {
        Err(Error(
            StatusCode::BAD_REQUEST,
            format!("{name:?} is not a secret's name: letters, digits, _ and -, at most 64"),
        ))
    }
}

/// A value as it is kept: without the whitespace a paste brings, not
/// empty, not huge.
fn cleaned(value: &str) -> Result<String, Error> {
    let v = value.trim();
    if v.is_empty() || v.len() > MAX_VALUE {
        return Err(Error(
            StatusCode::BAD_REQUEST,
            format!("a value: not empty, at most {} KiB", MAX_VALUE >> 10),
        ));
    }
    Ok(v.to_string())
}

/// The directory the values are in.
#[derive(Clone)]
pub struct Store(Arc<PathBuf>);

impl Store {
    pub fn open(dir: PathBuf) -> Result<Store> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        Ok(Store(Arc::new(dir)))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Whether zapper's entry is one of the workbench's: a file in here.
    fn owns(&self, entry: &Value) -> bool {
        entry["file"]
            .as_str()
            .is_some_and(|f| Path::new(f).parent() == Some(self.0.as_path()))
    }

    /// Writes `value` for `name` beside its file, to be moved into place
    /// once zapper has taken the entry.
    fn stage(&self, name: &str, value: &str) -> Result<PathBuf> {
        use std::io::Write as _;
        let tmp = self.0.join(format!(".{name}.new"));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(value.as_bytes())?;
        f.sync_all()?;
        Ok(tmp)
    }
}

/// The facility.
pub struct Secrets {
    store: Store,
}

impl Secrets {
    pub fn new(dir: PathBuf) -> Result<Secrets> {
        Ok(Secrets {
            store: Store::open(dir)?,
        })
    }
}

fn need(g: &Grant) -> Result<(), Error> {
    if g.may(Right::Secrets) {
        Ok(())
    } else {
        Err(Error(
            StatusCode::FORBIDDEN,
            format!("token {} may not manage secrets", g.name),
        ))
    }
}

/// zapper's running config's secrets, as it shows them (redacted).
async fn entries(ctx: &Ctx) -> Result<serde_json::Map<String, Value>, Error> {
    let c = ctx
        .zapper
        .call(Method::GET, "/v1/config?format=json", None)
        .await?
        .ok()?;
    Ok(c["secrets"].as_object().cloned().unwrap_or_default())
}

/// What uses secret `name`: profiles that write `{{secret:NAME}}`, and
/// rules that list it.
fn used_by(name: &str, config: &Value, rules: &Value) -> Vec<String> {
    let mut out = vec![];
    let placeholder = format!("{{{{secret:{name}}}}}");
    for (p, profile) in config["profiles"].as_object().into_iter().flatten() {
        if profile.to_string().contains(&placeholder) {
            out.push(format!("profile:{p}"));
        }
    }
    for r in rules.as_array().into_iter().flatten() {
        let lists = r["secrets"]
            .as_array()
            .is_some_and(|s| s.iter().any(|s| s == name));
        if lists {
            out.push(format!(
                "rule {} ({})",
                r["id"].as_str().unwrap_or("?"),
                r["scope"].as_str().unwrap_or("?")
            ));
        }
    }
    out
}

async fn list(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Extension(store): Extension<Store>,
) -> Result<Json<Value>, Error> {
    need(&g)?;
    let config = ctx
        .zapper
        .call(Method::GET, "/v1/config?format=json", None)
        .await?
        .ok()?;
    let rules = ctx
        .zapper
        .call(Method::GET, "/v1/rules", None)
        .await?
        .ok()?;
    let mut out = vec![];
    for (name, e) in config["secrets"].as_object().into_iter().flatten() {
        let source = ["file", "env", "command", "value"]
            .into_iter()
            .find(|k| e.get(*k).is_some())
            .unwrap_or("?");
        let mine = store.owns(e);
        out.push(json!({
            "name": name,
            "mode": e["mode"].as_str().unwrap_or("proxy"),
            "hosts": e["hosts"],
            "source": if mine { "workbench" } else { source },
            "workbench": mine,
            "has_value": !mine || store.path(name).is_file(),
            "used_by": used_by(name, &config, &rules),
            "plaintext": e["plaintext"].as_bool().unwrap_or(false),
            "user": e["user"],
            "database": e["database"],
            "mechanism": e["mechanism"],
        }));
    }
    Ok(Json(Value::Array(out)))
}

async fn presets(Extension(g): Extension<Grant>) -> Result<Json<Value>, Error> {
    need(&g)?;
    Ok(Json(Value::Array(
        PRESETS
            .iter()
            .map(|p| {
                json!({ "id": p.id, "title": p.title, "hosts": p.hosts, "mode": p.mode, "env": p.env, "hint": p.hint })
            })
            .collect(),
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetSecret {
    /// Fills in what isn't given: hosts, mode, and the value's check.
    preset: Option<String>,
    /// Needed for a new secret; kept as it is otherwise.
    value: Option<String>,
    hosts: Option<Vec<String>>,
    /// `proxy` (the default), `guest` (the session can read it), `aws`,
    /// `dbauth`.
    mode: Option<String>,
    #[serde(default)]
    plaintext: bool,
    user: Option<String>,
    database: Option<String>,
    mechanism: Option<String>,
}

/// Makes or changes secret `name`: its entry in zapper's running config,
/// and its value if one is given.
async fn set(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Extension(store): Extension<Store>,
    UrlPath(name): UrlPath<String>,
    Json(s): Json<SetSecret>,
) -> Result<Json<Value>, Error> {
    need(&g)?;
    let name = checked_name(&name)?.to_string();
    let bad = |why: String| Error(StatusCode::BAD_REQUEST, why);
    let preset = match &s.preset {
        Some(id) => Some(preset(id).ok_or_else(|| bad(format!("no preset {id}")))?),
        None => None,
    };
    let existing = entries(&ctx).await?.get(&name).cloned();
    if let Some(e) = &existing {
        if !store.owns(e) {
            return Err(Error(
                StatusCode::CONFLICT,
                format!("secret {name} is the config's own, not the workbench's: change it in the config"),
            ));
        }
    }
    let value = match &s.value {
        Some(v) => Some(cleaned(v)?),
        None if existing.is_none() => return Err(bad(format!("secret {name} needs a value"))),
        None => None,
    };
    if let (Some(p), Some(v)) = (preset, &value) {
        (p.check)(v).map_err(|why| bad(format!("that doesn't look right: {why}")))?;
    }
    let mode = s
        .mode
        .clone()
        .or(preset.map(|p| p.mode.to_string()))
        .or_else(|| {
            existing
                .as_ref()
                .and_then(|e| e["mode"].as_str().map(str::to_string))
        })
        .unwrap_or_else(|| "proxy".into());
    if mode == "aws" {
        if let Some(v) = &value {
            aws_json(v).map_err(bad)?;
        }
    }
    let hosts = s
        .hosts
        .clone()
        .or(preset
            .filter(|p| !p.hosts.is_empty())
            .map(|p| p.hosts.iter().map(|h| h.to_string()).collect()))
        .or_else(|| {
            existing
                .as_ref()
                .and_then(|e| serde_json::from_value(e["hosts"].clone()).ok())
        })
        .unwrap_or_default();
    let mut entry = json!({ "file": store.path(&name), "hosts": hosts, "mode": mode });
    for (k, v) in [
        ("user", &s.user),
        ("database", &s.database),
        ("mechanism", &s.mechanism),
    ] {
        if let Some(v) = v {
            entry[k] = json!(v);
        }
    }
    if s.plaintext {
        entry["plaintext"] = json!(true);
    }
    // The value is staged, the entry put, and only then the value moved
    // into place: a refused entry leaves nothing behind.
    let staged = match &value {
        Some(v) => Some(store.stage(&name, v)?),
        None => None,
    };
    let put = ctx
        .zapper
        .call(Method::PUT, &format!("/v1/secrets/{name}"), Some(&entry))
        .await
        .map_err(Error::from)
        .and_then(|a| a.ok().map_err(Error::from));
    if let Err(e) = put {
        if let Some(t) = staged {
            let _ = std::fs::remove_file(t);
        }
        return Err(e);
    }
    if let Some(t) = staged {
        std::fs::rename(t, store.path(&name)).map_err(anyhow::Error::from)?;
    }
    Ok(Json(
        json!({ "name": name, "mode": mode, "hosts": hosts, "saved": false }),
    ))
}

/// Removes secret `name`, the workbench's: zapper's entry (refused while
/// something uses it), then its value.
async fn remove(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Extension(store): Extension<Store>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<Value>, Error> {
    need(&g)?;
    let name = checked_name(&name)?.to_string();
    match entries(&ctx).await?.get(&name) {
        None => return Err(Error(StatusCode::NOT_FOUND, format!("no secret {name}"))),
        Some(e) if !store.owns(e) => {
            return Err(Error(
                StatusCode::CONFLICT,
                format!("secret {name} is the config's own, not the workbench's"),
            ))
        }
        Some(_) => {}
    }
    ctx.zapper
        .call(Method::DELETE, &format!("/v1/secrets/{name}"), None)
        .await?
        .ok()?;
    let _ = std::fs::remove_file(store.path(&name));
    Ok(Json(json!({ "removed": name })))
}

const PANEL: &str = include_str!("secrets.js");

impl Facility for Secrets {
    fn name(&self) -> &'static str {
        "secrets"
    }

    fn routes(&self) -> Router<Ctx> {
        Router::new()
            .route("/", get(list))
            .route("/presets", get(presets))
            .route("/{name}", put(set).delete(remove))
            .layer(Extension(self.store.clone()))
    }

    fn panel(&self) -> Option<Panel> {
        Some(Panel {
            title: "Secrets",
            script: PANEL,
        })
    }
}

/// Whether a rule names a secret, which takes the `secrets` right to add.
pub fn names_a_secret(rule: &Value) -> bool {
    rule["secrets"].as_array().is_some_and(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_check_what_a_value_looks_like() {
        let ok = |id: &str, v: &str| (preset(id).unwrap().check)(v).is_ok();
        assert!(ok("claude", "sk-ant-oat01-abc"));
        assert!(!ok("claude", "sk-or-v1-abc"));
        assert!(ok("openrouter", "sk-or-v1-abc"));
        assert!(ok("github", "github_pat_abc"));
        assert!(!ok("github", "password"));
        assert!(ok("aws", r#"{"AccessKeyId":"a","SecretAccessKey":"b"}"#));
        assert!(!ok("aws", r#"{"AccessKeyId":"a"}"#));
        assert!(ok("database", "anything"));
    }

    #[test]
    fn a_value_is_trimmed_and_bounded() {
        assert_eq!(cleaned("  tok\n").unwrap(), "tok");
        assert!(cleaned(" \n").is_err());
        assert!(cleaned(&"x".repeat(MAX_VALUE + 1)).is_err());
    }

    #[test]
    fn names_are_what_a_template_can_write() {
        assert!(checked_name("gh-token_2").is_ok());
        for bad in ["", "a b", "../x", "a/b", "x}}"] {
            assert!(checked_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn what_uses_a_secret_is_its_profiles_and_rules() {
        let config = json!({ "profiles": {
            "dev": { "agent": { "env": { "GITHUB_TOKEN": "{{secret:gh}}" } } },
            "other": { "agent": { "env": { "X": "{{secret:ghx}}" } } },
        } });
        let rules = json!([
            { "id": "g-1", "scope": "global", "secrets": ["gh"] },
            { "id": "g-2", "scope": "global", "secrets": ["claude"] },
        ]);
        assert_eq!(
            used_by("gh", &config, &rules),
            ["profile:dev", "rule g-1 (global)"]
        );
    }
}
